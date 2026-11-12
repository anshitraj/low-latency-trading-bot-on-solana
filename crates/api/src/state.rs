//! Process wiring: builds stores, RPC, senders, adapters, engine, observer.

use crate::appstore::{AppStore, MemAppStore};
use crate::config::Config;
use crate::providers::{default_policy, AppPolicies, AppSigners};
use crate::telegram::client::TelegramClient;
use parking_lot::Mutex;
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use trader_domain::{Network, SenderKind};
use trader_execution::{BlockhashService, ProtocolAdapter, WalletLocks};
use trader_market_data::{ws_url_from_http, AccountStream, RpcClient, RpcConfig};
use trader_observability::Metrics;
use trader_orders::balances::{RpcBalances, StreamBalances};
use trader_orders::{
    Engine, EngineConfig, MemStore, Observer, ObserverConfig, OrderStore, OrderUpdate,
};
use trader_router::{ManagedSender, Router, TokenBucket};
use trader_testkit::dex::Pool;
use trader_testkit::{ChainConfig, MockChain, MockDex};
use trader_wallets::Kek;

pub struct MockEnv {
    pub chain: MockChain,
    pub dex: MockDex,
    pub demo_mint: Pubkey,
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub engine: Arc<Engine>,
    pub orders: Arc<dyn OrderStore>,
    pub app: Arc<dyn AppStore>,
    pub rpc: RpcClient,
    pub tg: Option<TelegramClient>,
    pub kek: Option<Arc<Kek>>,
    pub metrics: Metrics,
    pub mock: Option<Arc<MockEnv>>,
    pub updates: broadcast::Sender<OrderUpdate>,
    pub senders: Vec<SenderKind>,
    pub rate: Arc<Mutex<HashMap<String, (u32, std::time::Instant)>>>,
    pub tg_queue: tokio::sync::mpsc::Sender<serde_json::Value>,
    pub mem_app: Option<MemAppStore>,
    pub outbox_notify: Arc<tokio::sync::Notify>,
}

impl AppState {
    /// Simple fixed-window limiter: `max` events per 60 s per key.
    pub fn allow(&self, key: &str, max: u32) -> bool {
        let mut g = self.rate.lock();
        let now = std::time::Instant::now();
        let e = g.entry(key.to_string()).or_insert((0, now));
        if now.duration_since(e.1) > Duration::from_secs(60) {
            *e = (0, now);
        }
        e.0 += 1;
        e.0 <= max
    }
}

pub struct Built {
    pub state: AppState,
    pub tg_rx: tokio::sync::mpsc::Receiver<serde_json::Value>,
    pub observer: Arc<Observer>,
}

pub async fn build(cfg: Config) -> anyhow::Result<Built> {
    let cfg = Arc::new(cfg);
    let metrics = Metrics::default();
    // --- chain access
    let (rpc, mock) = if cfg.mode == Network::Mock {
        let dex = MockDex::new();
        let demo_mint = Pubkey::new_from_array(*b"MOCKdemoTOKEN-not-a-real-mint!!!");
        dex.add_pool(
            demo_mint,
            Pool {
                sol_reserve: 30_000_000_000,
                token_reserve: 1_000_000_000_000_000,
                decimals: 6,
            },
        );
        let chain = MockChain::start(
            ChainConfig {
                slot_ms: cfg.mock_slot_ms,
                land_delay_slots: 1,
                confirm_after: 2,
                finalize_after: 6,
                blockhash_validity: 150,
            },
            dex.clone(),
        )
        .await;
        let mut rc = RpcConfig::new(chain.url.clone());
        rc.timeout = Duration::from_secs(3);
        (
            RpcClient::new(rc)?,
            Some(Arc::new(MockEnv {
                chain,
                dex,
                demo_mint,
            })),
        )
    } else {
        let mut rc = RpcConfig::new(cfg.rpc_url.clone().unwrap());
        rc.timeout = Duration::from_secs(5);
        let rpc = RpcClient::new(rc)?;
        let genesis = rpc.get_genesis_hash().await?;
        cfg.mode
            .verify_genesis(&genesis)
            .map_err(anyhow::Error::msg)?;
        (rpc, None)
    };
    // --- stores
    let (orders, app, mem_app): (Arc<dyn OrderStore>, Arc<dyn AppStore>, Option<MemAppStore>) =
        match &cfg.database_url {
            Some(url) => {
                let pg = Arc::new(crate::pg::PgStore::connect(url, cfg.mode).await?);
                (pg.clone(), pg, None)
            }
            None => {
                anyhow::ensure!(
                    cfg.mode != Network::Mainnet,
                    "mainnet requires DATABASE_URL"
                );
                let m = MemAppStore::default();
                (Arc::new(MemStore::new()), Arc::new(m.clone()), Some(m))
            }
        };
    // --- key encryption key (mock: ephemeral, logged as MOCK)
    let kek = match &cfg.kek {
        Some(k) => Some(Arc::new(Kek::new(cfg.kek_id.clone(), k)?)),
        None if cfg.mode == Network::Mock => {
            tracing::warn!(
                "MOCK mode: generating ephemeral KEK; managed wallets are lost on restart"
            );
            Some(Arc::new(Kek::new(
                "mock-ephemeral",
                trader_wallets::random_seed().as_ref(),
            )?))
        }
        None => None,
    };
    // --- senders
    let mut router = Router::new();
    let mut senders = vec![];
    router.add(ManagedSender::new(
        Arc::new(trader_sender_rpc::RpcSender::standard(
            rpc.clone(),
            cfg.skip_preflight,
        )),
        Duration::from_secs(5),
        None,
    ));
    senders.push(SenderKind::Rpc);
    if let Some(url) = &cfg.jito_url {
        let j = trader_sender_jito::JitoSender::new(
            url.clone(),
            cfg.jito_bundle_only,
            None,
            Duration::from_secs(5),
        );
        let accts = j.fetch_tip_accounts().await.unwrap_or_default();
        router.add(ManagedSender::new(
            Arc::new(j.with_tip_accounts(accts)),
            Duration::from_secs(5),
            Some(TokenBucket::new(
                cfg.jito_rps.max(1),
                Duration::from_secs(1),
            )),
        ));
        senders.push(SenderKind::Jito);
    }
    if let Some(url) = &cfg.helius_sender_url {
        let h = trader_sender_rpc::RpcSender::helius_sender(
            RpcClient::new(RpcConfig::new(url.clone()))?,
            cfg.helius_swqos_only,
        );
        router.add(ManagedSender::new(
            Arc::new(h),
            Duration::from_secs(5),
            Some(TokenBucket::new(1, Duration::from_secs(1))),
        ));
        senders.push(SenderKind::Helius);
    }
    // TPU sender is not built (see crates/sender-tpu); never registered.

    // --- WebSocket account stream (real clusters only; WS_STREAMING=false disables).
    // Coin state and wallet balances are pushed by the RPC; reads fall back to
    // RPC whenever the stream is not live or an account is not seeded yet.
    let stream = match (&mock, cfg.ws_streaming, &cfg.rpc_url) {
        (None, true, Some(http)) => Some(AccountStream::spawn(
            cfg.ws_url.clone().unwrap_or_else(|| ws_url_from_http(http)),
        )),
        _ => None,
    };
    // --- adapters
    let adapters: Vec<Arc<dyn ProtocolAdapter>> = match &mock {
        Some(m) => vec![Arc::new(m.dex.clone())],
        None => {
            let mut p = trader_protocol_pump::PumpAdapter::new(rpc.clone());
            if let Some(s) = &stream {
                p = p.with_stream(s.clone());
            }
            vec![Arc::new(p)]
        }
    };
    let balances: Arc<dyn trader_orders::BalanceSource> = match &stream {
        Some(s) => Arc::new(StreamBalances::new(rpc.clone(), s.clone())),
        None => Arc::new(RpcBalances { rpc: rpc.clone() }),
    };
    let engine_senders = senders.clone();
    let (tx, _) = broadcast::channel(8192);
    let locks = WalletLocks::default();
    let blockhash = BlockhashService::new();
    blockhash.refresh_once(&rpc).await?;
    blockhash.spawn(rpc.clone(), Duration::from_millis(500));
    let observer = Arc::new(Observer::new(
        orders.clone(),
        rpc.clone(),
        locks.clone(),
        tx.clone(),
        ObserverConfig {
            poll_interval: Duration::from_millis(cfg.observer_poll_ms),
            rebroadcast_interval: Duration::from_secs(2),
            max_rebroadcasts: 30,
        },
    ));
    let mut policy_senders = engine_senders.clone();
    if cfg.mode == Network::Mock {
        policy_senders.push(SenderKind::Mock);
    }
    let engine = Engine::new(
        EngineConfig {
            network: cfg.mode,
            mainnet_enabled: cfg.mainnet_trading_enabled,
            ..Default::default()
        },
        orders.clone(),
        adapters,
        router,
        blockhash,
        locks,
        Arc::new(AppSigners::new(app.clone(), kek.clone(), cfg.mode)),
        balances,
        Arc::new(AppPolicies {
            app: app.clone(),
            defaults: default_policy(cfg.mode, &policy_senders),
        }),
        observer.handle.clone(),
        tx.clone(),
    );
    engine.spawn_workers();
    let tg = cfg
        .bot_token
        .as_ref()
        .map(|t| TelegramClient::new(&cfg.telegram_api_base, t));
    let (tg_tx, tg_rx) = tokio::sync::mpsc::channel(1024);
    let state = AppState {
        cfg,
        engine,
        orders,
        app,
        rpc,
        tg,
        kek,
        metrics,
        mock,
        updates: tx,
        senders: engine_senders,
        rate: Default::default(),
        tg_queue: tg_tx,
        mem_app,
        outbox_notify: Arc::new(tokio::sync::Notify::new()),
    };
    Ok(Built {
        state,
        tg_rx,
        observer,
    })
}
