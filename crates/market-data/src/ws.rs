//! Solana RPC WebSocket account stream (`accountSubscribe` + `slotSubscribe`).
//!
//! Keeps the latest pushed state of subscribed accounts in memory so the hot
//! path can read it without an RPC round trip.
//!
//! Correctness rules (fail closed):
//! - An account entry is only served while its subscription has been
//!   acknowledged on the *current* connection and the entry was seeded
//!   (RPC read issued after the acknowledgement) or pushed since.
//! - Liveness is a slot heartbeat: if no `slotNotification` arrived within
//!   `max_silence`, [`AccountStream::is_live`] is false and callers must use RPC.
//! - On disconnect every entry and acknowledgement is dropped (updates may
//!   have been missed); after reconnect everything is resubscribed and must be
//!   re-seeded before it is served again.
//! - Newer slots win; a seed never overwrites a pushed update from a later slot.

use crate::rpc::{AccountData, RawAccount};
use futures::{SinkExt, StreamExt};
use parking_lot::{Mutex, RwLock};
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
pub struct StreamEntry {
    pub slot: u64,
    /// `None` = account does not exist (or was closed).
    pub account: Option<AccountData>,
    pub updated_at: Instant,
}

#[derive(Default)]
struct State {
    accounts: HashMap<Pubkey, StreamEntry>,
    /// Subscriptions acknowledged on the current connection.
    acked: HashSet<Pubkey>,
    /// sub id -> account, current connection.
    subs: HashMap<u64, Pubkey>,
}

enum Cmd {
    Accounts(Vec<Pubkey>),
    Signature(String),
}

struct Inner {
    url: String,
    wanted: RwLock<HashSet<Pubkey>>,
    /// Signatures awaiting a `processed` notification (resubscribed on reconnect).
    wanted_sigs: RwLock<HashSet<String>>,
    /// signature -> (slot, time the notification arrived)
    seen_sigs: Mutex<HashMap<String, (u64, Instant)>>,
    sig_notify: RwLock<Option<Arc<Notify>>>,
    state: RwLock<State>,
    cmd: mpsc::UnboundedSender<Cmd>,
    connected: AtomicBool,
    last_slot: AtomicU64,
    last_slot_at: Mutex<Option<Instant>>,
    reconnects: AtomicU64,
}

/// Cheap to clone; one background task per stream.
#[derive(Clone)]
pub struct AccountStream {
    inner: Arc<Inner>,
}

impl AccountStream {
    /// Spawn the connection task. `url` is `ws://` or `wss://` (may carry an
    /// API key; it is never logged).
    pub fn spawn(url: impl Into<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            url: url.into(),
            wanted: Default::default(),
            wanted_sigs: Default::default(),
            seen_sigs: Default::default(),
            sig_notify: Default::default(),
            state: Default::default(),
            cmd: tx,
            connected: AtomicBool::new(false),
            last_slot: AtomicU64::new(0),
            last_slot_at: Mutex::new(None),
            reconnects: AtomicU64::new(0),
        });
        tokio::spawn(run(inner.clone(), rx));
        Self { inner }
    }

    /// Host only, for logs/labels.
    pub fn host(&self) -> String {
        crate::rpc::url_host(&self.inner.url.replacen("ws", "http", 1))
    }

    /// Ask for these accounts to be streamed (idempotent).
    pub fn subscribe(&self, keys: &[Pubkey]) {
        let new: Vec<Pubkey> = {
            let mut w = self.inner.wanted.write();
            keys.iter().copied().filter(|k| w.insert(*k)).collect()
        };
        if !new.is_empty() {
            let _ = self.inner.cmd.send(Cmd::Accounts(new));
        }
    }

    /// One-shot `signatureSubscribe` at `processed`; on notification the
    /// signature is recorded (see [`Self::seen_signature`]) and the notifier set
    /// by [`Self::set_signature_notify`] is woken.
    pub fn subscribe_signature(&self, sig: &str) {
        if self.inner.wanted_sigs.write().insert(sig.to_string()) {
            let _ = self.inner.cmd.send(Cmd::Signature(sig.to_string()));
        }
    }

    pub fn set_signature_notify(&self, n: Arc<Notify>) {
        *self.inner.sig_notify.write() = Some(n);
    }

    /// (slot, arrival time) of the processed notification, if one arrived.
    pub fn seen_signature(&self, sig: &str) -> Option<(u64, Instant)> {
        self.inner.seen_sigs.lock().get(sig).copied()
    }

    pub fn is_acked(&self, key: &Pubkey) -> bool {
        self.inner.state.read().acked.contains(key)
    }

    /// Wait until every key's subscription is acknowledged on the current connection.
    pub async fn wait_acked(&self, keys: &[Pubkey], timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if keys.iter().all(|k| self.is_acked(k)) {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Seed an entry from an RPC read issued *after* the key was acknowledged.
    /// Ignored if not acknowledged (any update could have been missed) or if a
    /// newer slot is already stored.
    pub fn seed(&self, key: Pubkey, slot: u64, account: Option<AccountData>) -> bool {
        let mut st = self.inner.state.write();
        if !st.acked.contains(&key) {
            return false;
        }
        match st.accounts.get(&key) {
            Some(e) if e.slot > slot => false,
            _ => {
                st.accounts.insert(
                    key,
                    StreamEntry {
                        slot,
                        account,
                        updated_at: Instant::now(),
                    },
                );
                true
            }
        }
    }

    /// Latest entry, only if acknowledged on the current connection and seeded/pushed.
    pub fn get(&self, key: &Pubkey) -> Option<StreamEntry> {
        let st = self.inner.state.read();
        if !st.acked.contains(key) {
            return None;
        }
        st.accounts.get(key).cloned()
    }

    /// All keys at once (consistent read under one lock); `None` if any is missing.
    pub fn get_all(&self, keys: &[Pubkey]) -> Option<Vec<StreamEntry>> {
        let st = self.inner.state.read();
        keys.iter()
            .map(|k| {
                if st.acked.contains(k) {
                    st.accounts.get(k).cloned()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Connected and a slot notification arrived within `max_silence`.
    pub fn is_live(&self, max_silence: Duration) -> bool {
        self.inner.connected.load(Ordering::Acquire)
            && self
                .inner
                .last_slot_at
                .lock()
                .map(|t| t.elapsed() <= max_silence)
                .unwrap_or(false)
    }

    pub fn last_slot(&self) -> u64 {
        self.inner.last_slot.load(Ordering::Acquire)
    }

    pub fn reconnects(&self) -> u64 {
        self.inner.reconnects.load(Ordering::Relaxed)
    }
}

/// Derive the WebSocket URL from an HTTP RPC URL: http->ws, https->wss and the
/// solana-test-validator convention (RPC 8899 -> WS 8900).
pub fn ws_url_from_http(http: &str) -> String {
    let mut u = if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http.to_string()
    };
    if u.contains(":8899") {
        u = u.replacen(":8899", ":8900", 1);
    }
    u
}

/// Parsed server message (pure; unit-tested).
#[derive(Debug, PartialEq)]
pub(crate) enum WsEvent {
    /// Reply to our request id with a subscription id.
    Ack {
        req_id: u64,
        sub_id: u64,
    },
    Slot {
        slot: u64,
    },
    Account {
        sub_id: u64,
        slot: u64,
        account: Option<AccountData>,
    },
    Signature {
        sub_id: u64,
        slot: u64,
    },
    Other,
}

pub(crate) fn parse_message(text: &str) -> WsEvent {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return WsEvent::Other,
    };
    if let (Some(req_id), Some(sub_id)) = (v["id"].as_u64(), v["result"].as_u64()) {
        return WsEvent::Ack { req_id, sub_id };
    }
    match v["method"].as_str() {
        Some("slotNotification") => match v["params"]["result"]["slot"].as_u64() {
            Some(slot) => WsEvent::Slot { slot },
            None => WsEvent::Other,
        },
        Some("accountNotification") => {
            let p = &v["params"];
            let (Some(sub_id), Some(slot)) = (
                p["subscription"].as_u64(),
                p["result"]["context"]["slot"].as_u64(),
            ) else {
                return WsEvent::Other;
            };
            let value = &p["result"]["value"];
            let account = if value.is_null() {
                None
            } else {
                match serde_json::from_value::<RawAccount>(value.clone()).map(RawAccount::decode) {
                    Ok(Ok(a)) if a.lamports > 0 => Some(a),
                    Ok(Ok(_)) => None, // closed account (0 lamports)
                    _ => return WsEvent::Other,
                }
            };
            WsEvent::Account {
                sub_id,
                slot,
                account,
            }
        }
        Some("signatureNotification") => {
            let p = &v["params"];
            match (
                p["subscription"].as_u64(),
                p["result"]["context"]["slot"].as_u64(),
            ) {
                // `receivedSignature` notifications are not requested (no enableReceivedNotification).
                (Some(sub_id), Some(slot)) if p["result"]["value"].is_object() => {
                    WsEvent::Signature { sub_id, slot }
                }
                _ => WsEvent::Other,
            }
        }
        _ => WsEvent::Other,
    }
}

const SLOT_REQ_ID: u64 = 1;

async fn run(inner: Arc<Inner>, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut backoff = Duration::from_millis(250);
    loop {
        match tokio_tungstenite::connect_async(inner.url.as_str()).await {
            Ok((ws, _)) => {
                backoff = Duration::from_millis(250);
                session(&inner, ws, &mut rx).await;
            }
            Err(e) => {
                tracing::warn!(error = %crate::rpc::redact(&e.to_string()), "ws connect failed");
            }
        }
        // Drop everything: updates may have been missed while disconnected.
        inner.connected.store(false, Ordering::Release);
        {
            let mut st = inner.state.write();
            st.accounts.clear();
            st.acked.clear();
            st.subs.clear();
        }
        inner.reconnects.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

async fn session(
    inner: &Arc<Inner>,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
) {
    let (mut sink, mut stream) = ws.split();
    let mut next_id: u64 = SLOT_REQ_ID + 1;
    let mut pending: HashMap<u64, Pubkey> = HashMap::new();
    let mut pending_sigs: HashMap<u64, String> = HashMap::new();
    let mut sig_subs: HashMap<u64, String> = HashMap::new();
    let mut slot_sub: Option<u64> = None;
    let sig_msg = |id: u64, sig: &str| {
        json!({"jsonrpc":"2.0","id":id,"method":"signatureSubscribe",
               "params":[sig, {"commitment":"processed"}]})
        .to_string()
    };
    let sub_msg = |id: u64, k: &Pubkey| {
        json!({"jsonrpc":"2.0","id":id,"method":"accountSubscribe",
               "params":[k.to_string(), {"encoding":"base64","commitment":"processed"}]})
        .to_string()
    };
    if sink
        .send(Message::text(
            json!({"jsonrpc":"2.0","id":SLOT_REQ_ID,"method":"slotSubscribe"}).to_string(),
        ))
        .await
        .is_err()
    {
        return;
    }
    let wanted: Vec<Pubkey> = inner.wanted.read().iter().copied().collect();
    for k in wanted {
        let id = next_id;
        next_id += 1;
        pending.insert(id, k);
        if sink.send(Message::text(sub_msg(id, &k))).await.is_err() {
            return;
        }
    }
    let sigs: Vec<String> = inner.wanted_sigs.read().iter().cloned().collect();
    for sig in sigs {
        let id = next_id;
        next_id += 1;
        pending_sigs.insert(id, sig.clone());
        if sink.send(Message::text(sig_msg(id, &sig))).await.is_err() {
            return;
        }
    }
    inner.connected.store(true, Ordering::Release);
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    ping.tick().await;
    loop {
        tokio::select! {
            msg = stream.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Ping(p))) => {
                        if sink.send(Message::Pong(p)).await.is_err() { return; }
                        continue;
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => continue,
                };
                match parse_message(text.as_str()) {
                    WsEvent::Ack { req_id, sub_id } if req_id == SLOT_REQ_ID => slot_sub = Some(sub_id),
                    WsEvent::Ack { req_id, sub_id } if pending_sigs.contains_key(&req_id) => {
                        if let Some(sig) = pending_sigs.remove(&req_id) {
                            sig_subs.insert(sub_id, sig);
                        }
                    }
                    WsEvent::Ack { req_id, sub_id } => {
                        if let Some(k) = pending.remove(&req_id) {
                            let mut st = inner.state.write();
                            st.subs.insert(sub_id, k);
                            st.acked.insert(k);
                        }
                    }
                    WsEvent::Slot { slot } => {
                        let _ = slot_sub;
                        inner.last_slot.fetch_max(slot, Ordering::AcqRel);
                        *inner.last_slot_at.lock() = Some(Instant::now());
                    }
                    WsEvent::Account { sub_id, slot, account } => {
                        let mut st = inner.state.write();
                        if let Some(k) = st.subs.get(&sub_id).copied() {
                            let newer = st.accounts.get(&k).map(|e| slot >= e.slot).unwrap_or(true);
                            if newer {
                                st.accounts.insert(k, StreamEntry { slot, account, updated_at: Instant::now() });
                            }
                        }
                    }
                    WsEvent::Signature { sub_id, slot } => {
                        // One-shot: the server drops the subscription after notifying.
                        if let Some(sig) = sig_subs.remove(&sub_id) {
                            inner.wanted_sigs.write().remove(&sig);
                            let mut seen = inner.seen_sigs.lock();
                            if seen.len() > 10_000 {
                                seen.clear();
                            }
                            seen.insert(sig, (slot, Instant::now()));
                            drop(seen);
                            if let Some(n) = inner.sig_notify.read().as_ref() {
                                n.notify_one();
                            }
                        }
                    }
                    WsEvent::Other => {}
                }
            }
            cmd = rx.recv() => {
                match cmd {
                    None => return,
                    Some(Cmd::Accounts(keys)) => {
                        for k in keys {
                            let id = next_id;
                            next_id += 1;
                            pending.insert(id, k);
                            if sink.send(Message::text(sub_msg(id, &k))).await.is_err() { return; }
                        }
                    }
                    Some(Cmd::Signature(sig)) => {
                        let id = next_id;
                        next_id += 1;
                        pending_sigs.insert(id, sig.clone());
                        if sink.send(Message::text(sig_msg(id, &sig))).await.is_err() { return; }
                    }
                }
            }
            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() { return; }
            }
        }
    }
}

/// Keep `key`'s stream entry warm: subscribe, wait for the ack, then seed from
/// an RPC read issued after the ack (so no update can fall in between).
pub async fn warm(
    stream: &AccountStream,
    rpc: &crate::RpcClient,
    keys: &[Pubkey],
    timeout: Duration,
) -> Result<bool, crate::RpcError> {
    stream.subscribe(keys);
    if !stream.wait_acked(keys, timeout).await {
        return Ok(false);
    }
    let (slot, accts) = rpc
        .get_multiple_accounts(keys, trader_domain::Commitment::Processed)
        .await?;
    for (k, a) in keys.iter().zip(accts) {
        stream.seed(*k, slot, a);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_derivation() {
        assert_eq!(
            ws_url_from_http("http://127.0.0.1:8899"),
            "ws://127.0.0.1:8900"
        );
        assert_eq!(
            ws_url_from_http("https://api.mainnet-beta.solana.com"),
            "wss://api.mainnet-beta.solana.com"
        );
        assert_eq!(
            ws_url_from_http("https://mainnet.helius-rpc.com/?api-key=x"),
            "wss://mainnet.helius-rpc.com/?api-key=x"
        );
    }

    #[test]
    fn parses_ack_slot_account_and_closed() {
        assert_eq!(
            parse_message(r#"{"jsonrpc":"2.0","result":42,"id":7}"#),
            WsEvent::Ack {
                req_id: 7,
                sub_id: 42
            }
        );
        assert_eq!(
            parse_message(
                r#"{"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"parent":9,"root":1,"slot":10},"subscription":3}}"#
            ),
            WsEvent::Slot { slot: 10 }
        );
        let acct = r#"{"jsonrpc":"2.0","method":"accountNotification","params":{"result":{"context":{"slot":5},"value":{"lamports":7,"owner":"11111111111111111111111111111111","data":["AQID","base64"],"executable":false,"rentEpoch":0,"space":3}},"subscription":42}}"#;
        match parse_message(acct) {
            WsEvent::Account {
                sub_id,
                slot,
                account: Some(a),
            } => {
                assert_eq!(
                    (sub_id, slot, a.lamports, a.data.clone()),
                    (42, 5, 7, vec![1, 2, 3])
                );
            }
            e => panic!("{e:?}"),
        }
        let closed = acct.replace(r#""lamports":7"#, r#""lamports":0"#);
        assert!(matches!(
            parse_message(&closed),
            WsEvent::Account { account: None, .. }
        ));
        assert_eq!(parse_message("not json"), WsEvent::Other);
        assert_eq!(
            parse_message(
                r#"{"jsonrpc":"2.0","method":"signatureNotification","params":{"result":{"context":{"slot":77},"value":{"err":null}},"subscription":9}}"#
            ),
            WsEvent::Signature {
                sub_id: 9,
                slot: 77
            }
        );
    }

    #[tokio::test]
    async fn seed_requires_ack_and_newer_slot_wins() {
        let s = AccountStream::spawn("ws://127.0.0.1:9"); // never connects
        let k = Pubkey::new_unique();
        assert!(!s.seed(k, 10, None), "unacked seed must be ignored");
        s.inner.state.write().acked.insert(k);
        assert!(s.seed(k, 10, None));
        s.inner.state.write().accounts.get_mut(&k).unwrap().slot = 20;
        assert!(
            !s.seed(k, 15, None),
            "older seed must not overwrite newer push"
        );
        assert!(s.get(&k).is_some());
        assert!(
            !s.is_live(Duration::from_secs(5)),
            "no heartbeat -> not live"
        );
    }
}
