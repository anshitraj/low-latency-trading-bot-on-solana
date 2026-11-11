//! Shared harness: MockChain + MockDex + real engine/observer/RPC sender.

use async_trait::async_trait;
use parking_lot::Mutex;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use trader_domain::intent::FeePreference;
use trader_domain::{AmountSpec, Bps, Lamports, Network, OrderState, SenderKind, Side, TradeIntent, WalletPolicy};
use trader_execution::{BlockhashService, WalletLocks};
use trader_market_data::{RpcClient, RpcConfig};
use trader_orders::balances::RpcBalances;
use trader_orders::{Engine, EngineConfig, MemStore, Observer, ObserverConfig, Order, OrderStore, OrderUpdate, PolicySource, SignerProvider};
use trader_router::{ManagedSender, Router};
use trader_sender_rpc::RpcSender;
use trader_testkit::dex::Pool;
use trader_testkit::{ChainConfig, MockChain, MockDex};
use trader_wallets::{MockSigner, TxSigner};
use uuid::Uuid;

pub struct Signers(pub Mutex<HashMap<Uuid, Arc<dyn TxSigner>>>);

#[async_trait]
impl SignerProvider for Signers {
    async fn signer_for(&self, wallet_id: Uuid) -> Result<Arc<dyn TxSigner>, String> {
        self.0.lock().get(&wallet_id).cloned().ok_or_else(|| "no signer".into())
    }
}

pub struct Policies(pub Mutex<WalletPolicy>);

#[async_trait]
impl PolicySource for Policies {
    async fn policy(&self, _: Uuid) -> Result<WalletPolicy, String> {
        Ok(self.0.lock().clone())
    }
}

pub struct Harness {
    pub chain: MockChain,
    pub dex: MockDex,
    pub store: Arc<MemStore>,
    pub engine: Arc<Engine>,
    pub observer: Arc<Observer>,
    pub user_id: Uuid,
    pub wallet_id: Uuid,
    pub wallet: Pubkey,
    pub mint: Pubkey,
    pub policies: Arc<Policies>,
    pub updates: broadcast::Receiver<OrderUpdate>,
    pub rpc: RpcClient,
    pub locks: WalletLocks,
    pub blockhash: BlockhashService,
    pub router: Router,
    pub signers: Arc<Signers>,
}

pub struct Opts {
    pub chain: ChainConfig,
    pub observer: ObserverConfig,
    pub fund_lamports: u64,
    pub sender_timeout: Duration,
    pub start_observer: bool,
    pub workers: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            chain: ChainConfig::default(),
            observer: ObserverConfig { poll_interval: Duration::from_millis(15), rebroadcast_interval: Duration::from_millis(150), max_rebroadcasts: 20 },
            fund_lamports: 10_000_000_000,
            sender_timeout: Duration::from_secs(2),
            start_observer: true,
            workers: false,
        }
    }
}

pub fn test_policy() -> WalletPolicy {
    WalletPolicy {
        max_order: Lamports(1_000_000_000),
        max_daily_turnover: Lamports(100_000_000_000),
        max_trades_per_day: 10_000,
        allowed_senders: vec![SenderKind::Rpc, SenderKind::Mock, SenderKind::Jito],
        ..Default::default()
    }
}

impl Harness {
    pub async fn new(opts: Opts) -> Self {
        let dex = MockDex::new();
        let mint = Pubkey::new_unique();
        dex.add_pool(mint, Pool { sol_reserve: 100_000_000_000, token_reserve: 1_000_000_000_000_000, decimals: 6 });
        let chain = MockChain::start(opts.chain.clone(), dex.clone()).await;
        let signer = MockSigner::from_seed([42; 32]);
        let wallet = signer.keypair().pubkey();
        chain.fund(&wallet, opts.fund_lamports);
        let mut cfg = RpcConfig::new(chain.url.clone());
        cfg.timeout = opts.sender_timeout;
        let rpc = RpcClient::new(cfg).unwrap();
        let store = Arc::new(MemStore::new());
        let locks = WalletLocks::default();
        let blockhash = BlockhashService::new();
        blockhash.refresh_once(&rpc).await.unwrap();
        blockhash.spawn(rpc.clone(), Duration::from_millis(20));
        let mut router = Router::new();
        router.add(ManagedSender::new(Arc::new(RpcSender::standard(rpc.clone(), false)), opts.sender_timeout, None));
        let (tx, rx) = broadcast::channel(4096);
        let observer = Arc::new(Observer::new(store.clone(), rpc.clone(), locks.clone(), tx.clone(), opts.observer.clone()));
        if opts.start_observer {
            observer.clone().spawn();
        }
        let user_id = Uuid::new_v4();
        let wallet_id = Uuid::new_v4();
        let signers = Arc::new(Signers(Mutex::new(HashMap::from([(wallet_id, Arc::new(signer) as Arc<dyn TxSigner>)]))));
        let policies = Arc::new(Policies(Mutex::new(test_policy())));
        let engine = Engine::new(
            EngineConfig { network: Network::Mock, ..Default::default() },
            store.clone(),
            vec![Arc::new(dex.clone())],
            router.clone(),
            blockhash.clone(),
            locks.clone(),
            signers.clone(),
            Arc::new(RpcBalances { rpc: rpc.clone() }),
            policies.clone(),
            observer.handle.clone(),
            tx,
        );
        if opts.workers {
            engine.spawn_workers();
        }
        Self { chain, dex, store, engine, observer, user_id, wallet_id, wallet, mint, policies, updates: rx, rpc, locks, blockhash, router, signers }
    }

    pub fn intent(&self, side: Side, amount: AmountSpec, key: &str, approval: bool) -> TradeIntent {
        TradeIntent {
            id: Uuid::new_v4(),
            user_id: self.user_id,
            wallet_id: self.wallet_id,
            network: Network::Mock,
            side,
            mint: self.mint,
            amount,
            slippage: Bps::new(300).unwrap(),
            fees: FeePreference { compute_unit_price_micro_lamports: 1_000, max_priority_fee: Lamports(100_000), tip: Lamports(0) },
            senders: vec![SenderKind::Rpc],
            idempotency_key: key.to_string(),
            requires_approval: approval,
            created_at: chrono::Utc::now(),
        }
    }

    pub fn buy(&self, lamports: u64, key: &str) -> TradeIntent {
        self.intent(Side::Buy, AmountSpec::ExactSolIn { lamports: Lamports(lamports) }, key, false)
    }

    pub async fn wait_terminal(&self, id: Uuid, timeout: Duration) -> Order {
        let start = std::time::Instant::now();
        loop {
            let o = self.store.get_order(id).await.unwrap();
            if o.state.is_terminal() && (o.state != OrderState::Finalized || o.fill.is_some()) {
                return o;
            }
            if start.elapsed() > timeout {
                panic!("order {id} not terminal after {timeout:?}: {:?}", o.state);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub async fn wait_state(&self, id: Uuid, s: OrderState, timeout: Duration) -> Order {
        let start = std::time::Instant::now();
        loop {
            let o = self.store.get_order(id).await.unwrap();
            if o.state == s {
                return o;
            }
            if start.elapsed() > timeout {
                panic!("order {id} not {s} after {timeout:?}: {:?}", o.state);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}
