//! Direct TPU sender: QUIC straight to the upcoming leaders' TPU ports, no RPC
//! in the send path.
//!
//! - [`LeaderTracker`]: slot leaders (`getSlotLeaders`) and their QUIC TPU
//!   addresses (`getClusterNodes`.tpuQuic). The current slot comes from the
//!   WebSocket slot heartbeat when live, otherwise from `getSlot` plus elapsed
//!   time at 400 ms/slot.
//! - [`ConnCache`]: one QUIC connection per leader address, pre-warmed for
//!   leaders of the next few slots so a send does not pay the handshake.
//! - Each send writes the same signed bytes on a fresh unidirectional stream to
//!   the leaders of the next `fanout_slots` slots (deduplicated).
//!
//! Identity: a random, throwaway ed25519 key per process is used only for the
//! QUIC TLS handshake (validators require a client certificate). It is never
//! the wallet key and holds no funds.
//!
//! Limits (honest): this client is **unstaked**. Since stake-weighted QoS,
//! validators give unstaked connections a small share of TPU capacity; under
//! congestion packets may be dropped. Run it in parallel with a staked path
//! (Jito / Helius Sender / staked RPC); the router fans the same bytes out.
//! `Accepted` here means "written to a QUIC stream to at least one leader",
//! not "received by the scheduler" and never "landed".

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trader_domain::{Commitment, SenderKind};
use trader_market_data::{AccountStream, RpcClient};
use trader_router::{SendContext, SendOutcome, SenderHealth, TipPolicy, TransactionSender};

pub const ALPN_TPU: &[u8] = b"solana-tpu";
const SLOT_MS: u64 = 400;
const LEADER_WINDOW: u64 = 256;
const NODES_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct LeaderState {
    start_slot: u64,
    leaders: Vec<Pubkey>,
    tpu: HashMap<Pubkey, SocketAddr>,
    nodes_at: Option<Instant>,
    rpc_slot: u64,
    rpc_slot_at: Option<Instant>,
}

pub struct LeaderTracker {
    rpc: RpcClient,
    stream: Option<AccountStream>,
    st: RwLock<LeaderState>,
}

impl LeaderTracker {
    pub fn new(rpc: RpcClient, stream: Option<AccountStream>) -> Self {
        Self {
            rpc,
            stream,
            st: Default::default(),
        }
    }

    /// Best current-slot estimate (stream heartbeat if live, else RPC + elapsed).
    pub fn current_slot(&self) -> Option<u64> {
        if let Some(s) = &self.stream {
            if s.is_live(Duration::from_millis(1_500)) {
                return Some(s.last_slot());
            }
        }
        let st = self.st.read();
        st.rpc_slot_at
            .map(|t| st.rpc_slot + t.elapsed().as_millis() as u64 / SLOT_MS)
    }

    pub async fn refresh(&self) -> Result<(), String> {
        let live = self
            .stream
            .as_ref()
            .map(|s| s.is_live(Duration::from_millis(1_500)))
            .unwrap_or(false);
        if !live {
            let slot = self
                .rpc
                .get_slot(Commitment::Processed)
                .await
                .map_err(|e| e.to_string())?;
            let mut st = self.st.write();
            st.rpc_slot = slot;
            st.rpc_slot_at = Some(Instant::now());
        }
        let slot = self.current_slot().ok_or("no slot")?;
        let need_leaders = {
            let st = self.st.read();
            st.leaders.is_empty()
                || slot < st.start_slot
                || slot + 64 >= st.start_slot + st.leaders.len() as u64
        };
        if need_leaders {
            let v: Vec<String> = self
                .rpc
                .call("getSlotLeaders", json!([slot, LEADER_WINDOW]))
                .await
                .map_err(|e| e.to_string())?;
            let leaders = v.iter().filter_map(|s| Pubkey::from_str(s).ok()).collect();
            let mut st = self.st.write();
            st.start_slot = slot;
            st.leaders = leaders;
        }
        let need_nodes = self
            .st
            .read()
            .nodes_at
            .map(|t| t.elapsed() > NODES_TTL)
            .unwrap_or(true);
        if need_nodes {
            let nodes: Vec<Value> = self
                .rpc
                .call("getClusterNodes", json!([]))
                .await
                .map_err(|e| e.to_string())?;
            let mut tpu = HashMap::new();
            for n in nodes {
                let (Some(pk), Some(addr)) = (n["pubkey"].as_str(), n["tpuQuic"].as_str()) else {
                    continue;
                };
                if let (Ok(pk), Ok(addr)) = (Pubkey::from_str(pk), SocketAddr::from_str(addr)) {
                    tpu.insert(pk, addr);
                }
            }
            let mut st = self.st.write();
            st.tpu = tpu;
            st.nodes_at = Some(Instant::now());
        }
        Ok(())
    }

    /// Distinct (leader, TPU address) for slots `[current, current + n_slots)`, in slot order.
    pub fn upcoming(&self, n_slots: u64) -> Vec<(Pubkey, SocketAddr)> {
        let Some(cur) = self.current_slot() else {
            return vec![];
        };
        let st = self.st.read();
        let mut out: Vec<(Pubkey, SocketAddr)> = vec![];
        for s in cur..cur + n_slots {
            let Some(idx) = s.checked_sub(st.start_slot) else {
                continue;
            };
            let Some(pk) = st.leaders.get(idx as usize) else {
                continue;
            };
            if let Some(addr) = st.tpu.get(pk) {
                if !out.iter().any(|(p, _)| p == pk) {
                    out.push((*pk, *addr));
                }
            }
        }
        out
    }

    pub fn known_nodes(&self) -> usize {
        self.st.read().tpu.len()
    }
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Validators present self-signed identity certificates; there is no CA.
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// QUIC client config with a throwaway ed25519 identity and the TPU ALPN.
pub fn client_config() -> Result<quinn::ClientConfig, String> {
    let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).map_err(|e| e.to_string())?;
    let cert = rcgen::CertificateParams::new(vec!["solana".into()])
        .map_err(|e| e.to_string())?
        .self_signed(&kp)
        .map_err(|e| e.to_string())?;
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(kp.serialize_der());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification(provider)))
        .with_client_auth_cert(vec![cert.der().clone()], key.into())
        .map_err(|e| e.to_string())?;
    tls.alpn_protocols = vec![ALPN_TPU.to_vec()];
    let qc = quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(|e| e.to_string())?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(qc));
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).map_err(|e| e.to_string())?,
    ));
    cfg.transport_config(Arc::new(t));
    Ok(cfg)
}

pub struct ConnCache {
    endpoint: quinn::Endpoint,
    conns: Mutex<HashMap<SocketAddr, quinn::Connection>>,
    connect_timeout: Duration,
}

impl ConnCache {
    pub fn new(connect_timeout: Duration) -> Result<Self, String> {
        let mut endpoint = quinn::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))
            .map_err(|e| e.to_string())?;
        endpoint.set_default_client_config(client_config()?);
        Ok(Self {
            endpoint,
            conns: Default::default(),
            connect_timeout,
        })
    }

    fn cached(&self, addr: &SocketAddr) -> Option<quinn::Connection> {
        let g = self.conns.lock();
        g.get(addr).filter(|c| c.close_reason().is_none()).cloned()
    }

    /// Cached live connection or a new one (bounded by the connect timeout).
    pub async fn get(&self, addr: SocketAddr) -> Result<quinn::Connection, String> {
        if let Some(c) = self.cached(&addr) {
            return Ok(c);
        }
        let connecting = self
            .endpoint
            .connect(addr, "solana")
            .map_err(|e| e.to_string())?;
        let c = tokio::time::timeout(self.connect_timeout, connecting)
            .await
            .map_err(|_| "quic connect timeout".to_string())?
            .map_err(|e| e.to_string())?;
        self.conns.lock().insert(addr, c.clone());
        Ok(c)
    }

    /// Fresh handshake time to `addr` (diagnostics; replaces any cached connection).
    pub async fn handshake_time(&self, addr: SocketAddr) -> Result<Duration, String> {
        self.conns.lock().remove(&addr);
        let t = Instant::now();
        self.get(addr).await?;
        Ok(t.elapsed())
    }

    pub fn open(&self) -> usize {
        self.conns
            .lock()
            .values()
            .filter(|c| c.close_reason().is_none())
            .count()
    }
}

pub struct TpuSender {
    tracker: Arc<LeaderTracker>,
    conns: Arc<ConnCache>,
    fanout_slots: u64,
    last_error: Mutex<Option<String>>,
}

impl TpuSender {
    /// Builds the tracker + connection cache, does a first refresh and spawns
    /// the background refresh / pre-warm loop.
    pub async fn start(
        rpc: RpcClient,
        stream: Option<AccountStream>,
        fanout_slots: u64,
    ) -> Result<Arc<Self>, String> {
        let tracker = Arc::new(LeaderTracker::new(rpc, stream));
        tracker.refresh().await?;
        let conns = Arc::new(ConnCache::new(Duration::from_millis(1_500))?);
        let me = Arc::new(Self {
            tracker,
            conns,
            fanout_slots: fanout_slots.max(1),
            last_error: Mutex::new(None),
        });
        let bg = me.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(SLOT_MS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(e) = bg.tracker.refresh().await {
                    *bg.last_error.lock() = Some(e);
                    continue;
                }
                // Pre-warm connections to the leaders of the next few slots.
                for (_, addr) in bg.tracker.upcoming(bg.fanout_slots + 8) {
                    if bg.conns.cached(&addr).is_none() {
                        let c = bg.conns.clone();
                        tokio::spawn(async move {
                            let _ = c.get(addr).await;
                        });
                    }
                }
            }
        });
        Ok(me)
    }

    pub fn tracker(&self) -> &LeaderTracker {
        &self.tracker
    }

    pub fn conns(&self) -> &ConnCache {
        &self.conns
    }
}

async fn send_one(conns: &ConnCache, addr: SocketAddr, wire: &[u8]) -> Result<(), (bool, String)> {
    let c = conns.get(addr).await.map_err(|e| (false, e))?;
    let mut s = c.open_uni().await.map_err(|e| (false, e.to_string()))?;
    // From here bytes may have left the process.
    s.write_all(wire).await.map_err(|e| (true, e.to_string()))?;
    s.finish().map_err(|e| (true, e.to_string()))?;
    Ok(())
}

#[async_trait]
impl TransactionSender for TpuSender {
    fn kind(&self) -> SenderKind {
        SenderKind::Tpu
    }
    fn endpoint(&self) -> String {
        format!("tpu-quic:fanout{}", self.fanout_slots)
    }
    fn tip_policy(&self) -> TipPolicy {
        TipPolicy::None
    }
    async fn submit(&self, wire: &[u8], _ctx: &SendContext) -> SendOutcome {
        let targets = self.tracker.upcoming(self.fanout_slots);
        if targets.is_empty() {
            return SendOutcome::NotSent {
                reason: "no upcoming leader with a known TPU QUIC address".into(),
            };
        }
        let results = futures::future::join_all(
            targets
                .iter()
                .map(|(_, addr)| send_one(&self.conns, *addr, wire)),
        )
        .await;
        let ok = results.iter().filter(|r| r.is_ok()).count();
        if ok > 0 {
            return SendOutcome::Accepted {
                returned_signature: None,
            };
        }
        let partial = results.iter().any(|r| matches!(r, Err((true, _))));
        let reason = results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|(_, e)| e.clone()))
            .collect::<Vec<_>>()
            .join("; ");
        if partial {
            SendOutcome::Ambiguous { reason }
        } else {
            SendOutcome::NotSent { reason }
        }
    }
    async fn health(&self) -> SenderHealth {
        SenderHealth {
            configured: true,
            note: Some(format!(
                "leaders_known={} open_conns={} last_error={:?}",
                self.tracker.known_nodes(),
                self.conns.open(),
                self.last_error.lock().clone()
            )),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_builds_with_throwaway_identity() {
        client_config().expect("quic client config");
    }

    #[test]
    fn upcoming_dedupes_and_orders_by_slot() {
        let t = LeaderTracker::new(
            RpcClient::new(trader_market_data::RpcConfig::new("http://127.0.0.1:9")).unwrap(),
            None,
        );
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        {
            let mut st = t.st.write();
            st.start_slot = 100;
            st.leaders = vec![a, a, a, a, b, b, b, b];
            st.tpu.insert(a, "1.1.1.1:8009".parse().unwrap());
            st.tpu.insert(b, "2.2.2.2:8009".parse().unwrap());
            st.rpc_slot = 102;
            st.rpc_slot_at = Some(Instant::now());
        }
        let up = t.upcoming(4);
        assert_eq!(up.iter().map(|x| x.0).collect::<Vec<_>>(), vec![a, b]);
        assert_eq!(t.upcoming(1).len(), 1);
    }
}
