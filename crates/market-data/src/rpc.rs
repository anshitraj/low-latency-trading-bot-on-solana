//! Minimal, allocation-conscious Solana JSON-RPC client.
//!
//! Why hand-rolled instead of `solana-rpc-client`: we need (a) one persistent
//! keep-alive connection pool per endpoint, (b) precise error classification
//! (transport vs HTTP 429/5xx vs JSON-RPC error) for circuit breaking and
//! backoff, and (c) a small dependency surface. Only methods we use are here.
//! Method semantics follow https://solana.com/docs/rpc (accessed 2026-11-11).

use crate::pool::{EndpointSpec, EndpointStats, Health, RateLimiter};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_hash::Hash;
use solana_pubkey::Pubkey;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use trader_domain::Commitment;

#[derive(Debug, Clone, thiserror::Error)]
pub enum RpcError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("http status {status}: {body}")]
    Http { status: u16, body: String },
    #[error("rpc error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("decode error: {0}")]
    Decode(String),
}

impl RpcError {
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, RpcError::Http { status: 429, .. })
            || matches!(self, RpcError::Rpc { code: 429, .. })
    }
    /// Errors after which the request *may* have been processed by the server
    /// (so a send may have been forwarded). Callers must not treat these as
    /// "transaction rejected".
    pub fn is_ambiguous(&self) -> bool {
        matches!(
            self,
            RpcError::Transport(_)
                | RpcError::Timeout(_)
                | RpcError::Http {
                    status: 500..=599,
                    ..
                }
        )
    }
    pub fn is_retryable(&self) -> bool {
        self.is_rate_limited() || self.is_ambiguous()
    }
    /// JSON-RPC errors that describe what THIS endpoint can serve, not the
    /// request: another provider may answer it. Seen on mainnet 2026-11-13: our
    /// own node (no `--enable-rpc-transaction-history`) answered getTransaction
    /// with -32011 and the observer kept retrying it.
    /// -32011 transaction history not available, -32601 method not found,
    /// -32004 block not available for slot, -32009 missing in long-term storage.
    pub fn is_endpoint_capability(&self) -> bool {
        matches!(
            self,
            RpcError::Rpc {
                code: -32011 | -32601 | -32004 | -32009,
                ..
            }
        )
    }
}

#[derive(Clone, Debug)]
pub struct RpcConfig {
    pub url: String,
    pub timeout: Duration,
    /// Extra headers (e.g. provider auth). Never logged.
    pub headers: Vec<(String, String)>,
}

impl RpcConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: Duration::from_secs(5),
            headers: vec![],
        }
    }
}

/// One endpoint (single-provider client) or a pool of providers. A pool
/// spreads calls round-robin, rate-limits each provider client-side and fails
/// over on 429 / transport errors (see `crate::pool`).
#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<PoolInner>,
}

struct Endpoint {
    cfg: RpcConfig,
    http: reqwest::Client,
    limiter: Option<RateLimiter>,
    health: Health,
    stats: EndpointStats,
}

struct PoolInner {
    endpoints: Vec<Endpoint>,
    next: AtomicUsize,
    id: AtomicU64,
}

impl std::fmt::Debug for RpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // URLs may embed API keys; print hosts only.
        f.debug_struct("RpcClient")
            .field("hosts", &self.hosts())
            .finish()
    }
}

/// Per-endpoint counters (host only, never the URL).
#[derive(Clone, Debug, serde::Serialize)]
pub struct EndpointReport {
    pub host: String,
    pub ok: u64,
    pub rate_limited: u64,
    pub errors: u64,
}

pub fn url_host(u: &str) -> String {
    reqwest::Url::parse(u)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_else(|| "<invalid-url>".into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountData {
    pub lamports: u64,
    pub owner: Pubkey,
    pub data: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureStatus {
    pub slot: u64,
    pub confirmations: Option<u64>,
    /// JSON of the TransactionError if execution failed.
    pub err: Option<Value>,
    pub confirmation_status: Option<Commitment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBalanceChange {
    pub account_index: u64,
    pub mint: String,
    pub owner: Option<String>,
    pub pre_raw: u64,
    pub post_raw: u64,
    pub decimals: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionDetails {
    pub slot: u64,
    pub block_time: Option<i64>,
    pub fee: u64,
    pub err: Option<Value>,
    pub account_keys: Vec<String>,
    pub pre_balances: Vec<u64>,
    pub post_balances: Vec<u64>,
    pub token_balances: Vec<TokenBalanceChange>,
    pub compute_units_consumed: Option<u64>,
    pub log_messages: Vec<String>,
}

impl TransactionDetails {
    /// Lamport change of a given account (post - pre), including fees.
    pub fn lamport_delta(&self, key: &str) -> Option<i128> {
        let i = self.account_keys.iter().position(|k| k == key)?;
        Some(*self.post_balances.get(i)? as i128 - *self.pre_balances.get(i)? as i128)
    }
    /// Raw token change for (owner, mint), summed across that owner's accounts.
    pub fn token_delta(&self, owner: &str, mint: &str) -> i128 {
        self.token_balances
            .iter()
            .filter(|t| t.mint == mint && t.owner.as_deref() == Some(owner))
            .map(|t| t.post_raw as i128 - t.pre_raw as i128)
            .sum()
    }
}

#[derive(Deserialize)]
struct RpcEnvelope {
    result: Option<Value>,
    error: Option<RpcErrorObj>,
}
#[derive(Deserialize)]
struct RpcErrorObj {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl RpcClient {
    fn endpoint(cfg: RpcConfig, rps: Option<f64>) -> Result<Endpoint, RpcError> {
        let http = reqwest::Client::builder()
            .tcp_nodelay(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Duration::from_secs(30))
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(Endpoint {
            cfg,
            http,
            limiter: rps.map(RateLimiter::new),
            health: Health::default(),
            stats: EndpointStats::default(),
        })
    }

    /// Single endpoint, no client-side rate limit (tests, local validator, senders).
    pub fn new(cfg: RpcConfig) -> Result<Self, RpcError> {
        Ok(Self {
            inner: Arc::new(PoolInner {
                endpoints: vec![Self::endpoint(cfg, None)?],
                next: AtomicUsize::new(0),
                id: AtomicU64::new(1),
            }),
        })
    }

    /// Pool of providers, each rate-limited client-side (default
    /// `pool::DEFAULT_RPS` req/s unless the spec says otherwise).
    pub fn pool(specs: &[EndpointSpec], timeout: Duration) -> Result<Self, RpcError> {
        if specs.is_empty() {
            return Err(RpcError::Decode("no RPC endpoints configured".into()));
        }
        let mut endpoints = vec![];
        for s in specs {
            let mut cfg = RpcConfig::new(s.http.clone());
            cfg.timeout = timeout;
            endpoints.push(Self::endpoint(
                cfg,
                Some(s.rps.unwrap_or(crate::pool::DEFAULT_RPS)),
            )?);
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                endpoints,
                next: AtomicUsize::new(0),
                id: AtomicU64::new(1),
            }),
        })
    }

    /// Host of the first endpoint (`+N` when pooled). Never the URL.
    pub fn host(&self) -> String {
        let eps = &self.inner.endpoints;
        let h = url_host(&eps[0].cfg.url);
        if eps.len() > 1 {
            format!("{h}+{}", eps.len() - 1)
        } else {
            h
        }
    }

    pub fn hosts(&self) -> Vec<String> {
        self.inner
            .endpoints
            .iter()
            .map(|e| url_host(&e.cfg.url))
            .collect()
    }

    pub fn endpoint_count(&self) -> usize {
        self.inner.endpoints.len()
    }

    pub fn stats(&self) -> Vec<EndpointReport> {
        self.inner
            .endpoints
            .iter()
            .map(|e| EndpointReport {
                host: url_host(&e.cfg.url),
                ok: e.stats.ok.load(Ordering::Relaxed),
                rate_limited: e.stats.rate_limited.load(Ordering::Relaxed),
                errors: e.stats.errors.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Pick an endpoint: round-robin over those not cooling down whose limiter
    /// grants a token; otherwise wait (bounded by the request timeout).
    async fn pick(&self, critical: bool, tried: &[bool]) -> Option<usize> {
        let eps = &self.inner.endpoints;
        let n = eps.len();
        let start = self.inner.next.fetch_add(1, Ordering::Relaxed) % n;
        let deadline = Instant::now() + eps[0].cfg.timeout;
        loop {
            let mut wait = Duration::from_millis(25);
            let all_tried = tried.iter().all(|t| *t);
            for i in 0..n {
                let idx = (start + i) % n;
                if tried[idx] && !all_tried {
                    continue;
                }
                let ep = &eps[idx];
                if let Some(c) = ep.health.cooling_for() {
                    wait = wait.min(c);
                    continue;
                }
                match &ep.limiter {
                    None => return Some(idx),
                    Some(l) => match l.try_take(critical) {
                        Ok(()) => return Some(idx),
                        Err(w) => wait = wait.min(w),
                    },
                }
            }
            if Instant::now() + wait > deadline {
                return None;
            }
            tokio::time::sleep(wait.max(Duration::from_millis(1))).await;
        }
    }

    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let n = self.inner.endpoints.len();
        if n == 1 {
            let ep = &self.inner.endpoints[0];
            if let Some(l) = &ep.limiter {
                while let Err(w) = l.try_take(crate::pool::is_critical(method)) {
                    tokio::time::sleep(w).await;
                }
            }
            return self.call_on(0, method, &params).await;
        }
        let critical = crate::pool::is_critical(method);
        // Sends never fail over automatically (the sender owns ambiguity).
        let max_attempts = if method == "sendTransaction" {
            1
        } else {
            n.min(4)
        };
        let mut tried = vec![false; n];
        let mut last_err = None;
        for _ in 0..max_attempts {
            let Some(idx) = self.pick(critical, &tried).await else {
                break;
            };
            tried[idx] = true;
            match self.call_on(idx, method, &params).await {
                Ok(v) => return Ok(v),
                // This endpoint cannot serve the method: ask the next one.
                Err(e) if e.is_endpoint_capability() => last_err = Some(e),
                Err(e) if matches!(e, RpcError::Rpc { .. }) && !e.is_rate_limited() => {
                    return Err(e)
                }
                Err(e) if e.is_retryable() => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| RpcError::Http {
            status: 429,
            body: "all RPC endpoints rate-limited or cooling down".into(),
        }))
    }

    async fn call_on(&self, idx: usize, method: &str, params: &Value) -> Result<Value, RpcError> {
        let ep = &self.inner.endpoints[idx];
        let r = self.call_endpoint(ep, method, params).await;
        match &r {
            Ok(_) => {
                ep.health.ok();
                ep.stats.ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) if e.is_rate_limited() => {
                ep.health.strike();
                ep.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) if e.is_ambiguous() => {
                ep.health.strike();
                ep.stats.errors.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                ep.stats.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        r
    }

    async fn call_endpoint(
        &self,
        ep: &Endpoint,
        method: &str,
        params: &Value,
    ) -> Result<Value, RpcError> {
        let id = self.inner.id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let mut req = ep.http.post(&ep.cfg.url).json(&body);
        for (k, v) in &ep.cfg.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                RpcError::Timeout(ep.cfg.timeout)
            } else {
                RpcError::Transport(redact(&e.to_string()))
            }
        })?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| {
            if e.is_timeout() {
                RpcError::Timeout(ep.cfg.timeout)
            } else {
                RpcError::Transport(redact(&e.to_string()))
            }
        })?;
        if status != 200 {
            let body = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_string();
            return Err(RpcError::Http { status, body });
        }
        let env: RpcEnvelope =
            serde_json::from_slice(&bytes).map_err(|e| RpcError::Decode(e.to_string()))?;
        if let Some(e) = env.error {
            return Err(RpcError::Rpc {
                code: e.code,
                message: e.message,
                data: e.data,
            });
        }
        env.result
            .ok_or_else(|| RpcError::Decode("missing result".into()))
    }

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let v = self.call_raw(method, params).await?;
        serde_json::from_value(v).map_err(|e| RpcError::Decode(format!("{method}: {e}")))
    }

    pub async fn get_genesis_hash(&self) -> Result<String, RpcError> {
        self.call("getGenesisHash", json!([])).await
    }

    pub async fn get_health(&self) -> Result<String, RpcError> {
        self.call("getHealth", json!([])).await
    }

    pub async fn get_slot(&self, c: Commitment) -> Result<u64, RpcError> {
        self.call("getSlot", json!([{"commitment": c.as_str()}]))
            .await
    }

    pub async fn get_block_height(&self, c: Commitment) -> Result<u64, RpcError> {
        self.call("getBlockHeight", json!([{"commitment": c.as_str()}]))
            .await
    }

    /// Returns (blockhash, last_valid_block_height, context_slot).
    pub async fn get_latest_blockhash(&self, c: Commitment) -> Result<(Hash, u64, u64), RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct V {
            blockhash: String,
            last_valid_block_height: u64,
        }
        #[derive(Deserialize)]
        struct R {
            context: Ctx,
            value: V,
        }
        let r: R = self
            .call("getLatestBlockhash", json!([{"commitment": c.as_str()}]))
            .await?;
        let h = Hash::from_str(&r.value.blockhash).map_err(|e| RpcError::Decode(e.to_string()))?;
        Ok((h, r.value.last_valid_block_height, r.context.slot))
    }

    pub async fn get_balance(&self, key: &Pubkey, c: Commitment) -> Result<(u64, u64), RpcError> {
        #[derive(Deserialize)]
        struct R {
            context: Ctx,
            value: u64,
        }
        let r: R = self
            .call(
                "getBalance",
                json!([key.to_string(), {"commitment": c.as_str()}]),
            )
            .await?;
        Ok((r.value, r.context.slot))
    }

    /// Returns (context_slot, accounts) in request order.
    pub async fn get_multiple_accounts(
        &self,
        keys: &[Pubkey],
        c: Commitment,
    ) -> Result<(u64, Vec<Option<AccountData>>), RpcError> {
        if keys.len() > 100 {
            return Err(RpcError::Decode(
                "getMultipleAccounts supports at most 100 keys".into(),
            ));
        }
        let ks: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        #[derive(Deserialize)]
        struct R {
            context: Ctx,
            value: Vec<Option<RawAccount>>,
        }
        let r: R = self
            .call(
                "getMultipleAccounts",
                json!([ks, {"commitment": c.as_str(), "encoding": "base64"}]),
            )
            .await?;
        let accts = r
            .value
            .into_iter()
            .map(|a| a.map(RawAccount::decode).transpose())
            .collect::<Result<_, _>>()?;
        Ok((r.context.slot, accts))
    }

    pub async fn get_account(
        &self,
        key: &Pubkey,
        c: Commitment,
    ) -> Result<(u64, Option<AccountData>), RpcError> {
        let (slot, mut v) = self
            .get_multiple_accounts(std::slice::from_ref(key), c)
            .await?;
        Ok((slot, v.pop().flatten()))
    }

    /// Sends a pre-signed, serialized transaction. Returns the signature the
    /// node reports (must equal the locally computed one; callers verify).
    pub async fn send_transaction(
        &self,
        wire_tx: &[u8],
        skip_preflight: bool,
        max_retries: Option<u32>,
        preflight_commitment: Commitment,
    ) -> Result<String, RpcError> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(wire_tx);
        let mut cfg = json!({
            "encoding": "base64",
            "skipPreflight": skip_preflight,
            "preflightCommitment": preflight_commitment.as_str(),
        });
        if let Some(r) = max_retries {
            cfg["maxRetries"] = json!(r);
        }
        self.call("sendTransaction", json!([b64, cfg])).await
    }

    pub async fn simulate_transaction(
        &self,
        wire_tx: &[u8],
        c: Commitment,
    ) -> Result<Value, RpcError> {
        self.simulate_transaction_opts(wire_tx, c, true).await
    }

    /// `sig_verify = false` lets callers simulate bytes whose signatures were
    /// zeroed, so the payload sent to the RPC can never be broadcast.
    pub async fn simulate_transaction_opts(
        &self,
        wire_tx: &[u8],
        c: Commitment,
        sig_verify: bool,
    ) -> Result<Value, RpcError> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(wire_tx);
        self.call(
            "simulateTransaction",
            json!([b64, {"encoding":"base64","commitment": c.as_str(), "sigVerify": sig_verify, "replaceRecentBlockhash": false}]),
        )
        .await
    }

    pub async fn get_signature_statuses(
        &self,
        sigs: &[String],
        search_history: bool,
    ) -> Result<(u64, Vec<Option<SignatureStatus>>), RpcError> {
        if sigs.len() > 256 {
            return Err(RpcError::Decode(
                "getSignatureStatuses supports at most 256".into(),
            ));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct S {
            slot: u64,
            confirmations: Option<u64>,
            err: Option<Value>,
            confirmation_status: Option<String>,
        }
        #[derive(Deserialize)]
        struct R {
            context: Ctx,
            value: Vec<Option<S>>,
        }
        let r: R = self
            .call(
                "getSignatureStatuses",
                json!([sigs, {"searchTransactionHistory": search_history}]),
            )
            .await?;
        let v = r
            .value
            .into_iter()
            .map(|s| {
                s.map(|s| SignatureStatus {
                    slot: s.slot,
                    confirmations: s.confirmations,
                    err: s.err.filter(|e| !e.is_null()),
                    confirmation_status: s
                        .confirmation_status
                        .as_deref()
                        .and_then(parse_commitment),
                })
            })
            .collect();
        Ok((r.context.slot, v))
    }

    pub async fn get_transaction(
        &self,
        sig: &str,
        c: Commitment,
    ) -> Result<Option<TransactionDetails>, RpcError> {
        if c == Commitment::Processed {
            return Err(RpcError::Decode(
                "getTransaction requires confirmed or finalized".into(),
            ));
        }
        let v = self
            .call_raw(
                "getTransaction",
                json!([sig, {"commitment": c.as_str(), "encoding": "json", "maxSupportedTransactionVersion": 0}]),
            )
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        parse_transaction(&v).map(Some)
    }

    pub async fn request_airdrop(&self, key: &Pubkey, lamports: u64) -> Result<String, RpcError> {
        self.call("requestAirdrop", json!([key.to_string(), lamports]))
            .await
    }

    /// Recent prioritization fees (micro-lamports per CU) for the given writable accounts.
    pub async fn get_recent_prioritization_fees(
        &self,
        keys: &[Pubkey],
    ) -> Result<Vec<(u64, u64)>, RpcError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct F {
            slot: u64,
            prioritization_fee: u64,
        }
        let ks: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
        let v: Vec<F> = self
            .call("getRecentPrioritizationFees", json!([ks]))
            .await?;
        Ok(v.into_iter()
            .map(|f| (f.slot, f.prioritization_fee))
            .collect())
    }

    /// (address, raw amount) of the largest token holders.
    pub async fn get_token_largest_accounts(
        &self,
        mint: &Pubkey,
    ) -> Result<Vec<(String, u64)>, RpcError> {
        #[derive(Deserialize)]
        struct A {
            address: String,
            amount: String,
        }
        #[derive(Deserialize)]
        struct R {
            value: Vec<A>,
        }
        let r: R = self
            .call(
                "getTokenLargestAccounts",
                json!([mint.to_string(), {"commitment":"confirmed"}]),
            )
            .await?;
        r.value
            .into_iter()
            .map(|a| {
                Ok((
                    a.address,
                    a.amount
                        .parse()
                        .map_err(|_| RpcError::Decode("amount".into()))?,
                ))
            })
            .collect()
    }

    /// Signatures touching an address, newest first.
    pub async fn get_signatures_for_address(
        &self,
        addr: &Pubkey,
        until: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, u64, bool)>, RpcError> {
        #[derive(Deserialize)]
        struct S {
            signature: String,
            slot: u64,
            err: Option<Value>,
        }
        let mut cfg = json!({"limit": limit.min(1000), "commitment": "confirmed"});
        if let Some(u) = until {
            cfg["until"] = json!(u);
        }
        let v: Vec<S> = self
            .call("getSignaturesForAddress", json!([addr.to_string(), cfg]))
            .await?;
        Ok(v.into_iter()
            .map(|s| {
                (
                    s.signature,
                    s.slot,
                    s.err.map(|e| !e.is_null()).unwrap_or(false),
                )
            })
            .collect())
    }
}

#[derive(Deserialize)]
struct Ctx {
    slot: u64,
}

#[derive(Deserialize)]
pub(crate) struct RawAccount {
    lamports: u64,
    owner: String,
    data: (String, String),
    executable: bool,
}

impl RawAccount {
    pub(crate) fn decode(self) -> Result<AccountData, RpcError> {
        if self.data.1 != "base64" {
            return Err(RpcError::Decode(format!(
                "unexpected encoding {}",
                self.data.1
            )));
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(self.data.0)
            .map_err(|e| RpcError::Decode(e.to_string()))?;
        Ok(AccountData {
            lamports: self.lamports,
            owner: Pubkey::from_str(&self.owner).map_err(|e| RpcError::Decode(e.to_string()))?,
            data,
            executable: self.executable,
        })
    }
}

pub fn parse_commitment(s: &str) -> Option<Commitment> {
    match s {
        "processed" => Some(Commitment::Processed),
        "confirmed" => Some(Commitment::Confirmed),
        "finalized" => Some(Commitment::Finalized),
        _ => None,
    }
}

pub(crate) fn redact(s: &str) -> String {
    // reqwest errors include the URL; strip query strings which may carry api keys.
    let mut out = String::with_capacity(s.len());
    let mut in_query = false;
    for ch in s.chars() {
        if ch == '?' {
            in_query = true;
            out.push_str("?<redacted>");
            continue;
        }
        if in_query && (ch == ')' || ch == ' ') {
            in_query = false;
        }
        if !in_query {
            out.push(ch);
        }
    }
    out
}

pub fn parse_transaction(v: &Value) -> Result<TransactionDetails, RpcError> {
    let d = |m: &str| RpcError::Decode(format!("getTransaction: {m}"));
    let meta = v.get("meta").ok_or_else(|| d("meta"))?;
    let msg = v
        .pointer("/transaction/message")
        .ok_or_else(|| d("message"))?;
    let mut account_keys: Vec<String> = msg
        .get("accountKeys")
        .and_then(|a| a.as_array())
        .ok_or_else(|| d("accountKeys"))?
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    // v0: loaded addresses are appended writable then readonly
    if let Some(la) = meta.get("loadedAddresses") {
        for part in ["writable", "readonly"] {
            if let Some(arr) = la.get(part).and_then(|a| a.as_array()) {
                account_keys.extend(arr.iter().filter_map(|k| k.as_str().map(str::to_string)));
            }
        }
    }
    let u64s = |key: &str| -> Vec<u64> {
        meta.get(key)
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
            .unwrap_or_default()
    };
    let parse_tb = |key: &str| -> Vec<(u64, String, Option<String>, u64, u8)> {
        meta.get(key)
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| {
                        Some((
                            t.get("accountIndex")?.as_u64()?,
                            t.get("mint")?.as_str()?.to_string(),
                            t.get("owner").and_then(|o| o.as_str()).map(str::to_string),
                            t.pointer("/uiTokenAmount/amount")?.as_str()?.parse().ok()?,
                            t.pointer("/uiTokenAmount/decimals")?.as_u64()? as u8,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let pre = parse_tb("preTokenBalances");
    let post = parse_tb("postTokenBalances");
    let mut token_balances: Vec<TokenBalanceChange> = Vec::new();
    for (idx, mint, owner, amt, dec) in &post {
        let pre_raw = pre.iter().find(|p| p.0 == *idx).map(|p| p.3).unwrap_or(0);
        token_balances.push(TokenBalanceChange {
            account_index: *idx,
            mint: mint.clone(),
            owner: owner.clone(),
            pre_raw,
            post_raw: *amt,
            decimals: *dec,
        });
    }
    for (idx, mint, owner, amt, dec) in &pre {
        if !post.iter().any(|p| p.0 == *idx) {
            token_balances.push(TokenBalanceChange {
                account_index: *idx,
                mint: mint.clone(),
                owner: owner.clone(),
                pre_raw: *amt,
                post_raw: 0,
                decimals: *dec,
            });
        }
    }
    Ok(TransactionDetails {
        slot: v
            .get("slot")
            .and_then(|s| s.as_u64())
            .ok_or_else(|| d("slot"))?,
        block_time: v.get("blockTime").and_then(|b| b.as_i64()),
        fee: meta
            .get("fee")
            .and_then(|f| f.as_u64())
            .ok_or_else(|| d("fee"))?,
        err: meta.get("err").cloned().filter(|e| !e.is_null()),
        account_keys,
        pre_balances: u64s("preBalances"),
        post_balances: u64s("postBalances"),
        token_balances,
        compute_units_consumed: meta.get("computeUnitsConsumed").and_then(|c| c.as_u64()),
        log_messages: meta
            .get("logMessages")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    async fn serve(router: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn pool_fails_over_from_rate_limited_endpoint_and_counts_it() {
        let bad = serve(Router::new().route(
            "/",
            post(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(json!({}))) }),
        ))
        .await;
        let good = serve(Router::new().route(
            "/",
            post(|| async { Json(json!({"jsonrpc":"2.0","id":1,"result":7})) }),
        ))
        .await;
        let specs = crate::pool::parse_endpoint_lines(&format!(
            "{bad}
{good}
"
        ));
        let pool = RpcClient::pool(&specs, Duration::from_secs(2)).unwrap();
        assert_eq!(pool.endpoint_count(), 2);
        for _ in 0..4 {
            assert_eq!(pool.get_slot(Commitment::Processed).await.unwrap(), 7);
        }
        let st = pool.stats();
        assert!(st[0].rate_limited >= 1, "{st:?}");
        assert_eq!(st[1].ok, 4, "{st:?}");
        // While cooling down, the bad endpoint is skipped (fewer 429s than calls).
        assert!(st[0].rate_limited < 4, "{st:?}");
    }

    #[tokio::test]
    async fn pool_fails_over_when_endpoint_lacks_the_capability() {
        let no_history = serve(Router::new().route(
            "/",
            post(|| async {
                Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32011,"message":"Transaction history is not available from this node"}}))
            }),
        ))
        .await;
        let good = serve(Router::new().route(
            "/",
            post(|| async { Json(json!({"jsonrpc":"2.0","id":1,"result":7})) }),
        ))
        .await;
        let specs = crate::pool::parse_endpoint_lines(&format!("{no_history}\n{good}\n"));
        let pool = RpcClient::pool(&specs, Duration::from_secs(2)).unwrap();
        for _ in 0..4 {
            assert_eq!(pool.get_slot(Commitment::Processed).await.unwrap(), 7);
        }
        // A deterministic per-request error is still returned as-is.
        let bad_req = serve(Router::new().route(
            "/",
            post(|| async {
                Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"invalid params"}}))
            }),
        ))
        .await;
        let specs = crate::pool::parse_endpoint_lines(&format!("{bad_req}\n{good}\n"));
        let pool = RpcClient::pool(&specs, Duration::from_secs(2)).unwrap();
        let mut saw_err = false;
        for _ in 0..4 {
            if let Err(RpcError::Rpc { code: -32602, .. }) =
                pool.get_slot(Commitment::Processed).await
            {
                saw_err = true;
            }
        }
        assert!(saw_err, "invalid params must not be masked by failover");
    }

    #[tokio::test]
    async fn pool_does_not_fail_over_sends() {
        let bad = serve(Router::new().route(
            "/",
            post(|| async { (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(json!({}))) }),
        ))
        .await;
        let good = serve(Router::new().route(
            "/",
            post(|| async { Json(json!({"jsonrpc":"2.0","id":1,"result":"SIG"})) }),
        ))
        .await;
        let specs = crate::pool::parse_endpoint_lines(&format!(
            "{bad}
{good}
"
        ));
        let pool = RpcClient::pool(&specs, Duration::from_secs(2)).unwrap();
        let mut errs = 0;
        for _ in 0..2 {
            if pool
                .send_transaction(&[1, 2, 3], false, Some(0), Commitment::Processed)
                .await
                .is_err()
            {
                errs += 1;
            }
        }
        // Round-robin: one send hits the 429 endpoint and is NOT retried elsewhere.
        assert_eq!(errs, 1);
    }

    #[tokio::test]
    async fn classifies_errors() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let app = Router::new().route(
            "/",
            post(move |Json(body): Json<Value>| {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    let m = body["method"].as_str().unwrap().to_string();
                    match m.as_str() {
                        "getSlot" => (axum::http::StatusCode::OK, Json(json!({"jsonrpc":"2.0","id":1,"result":42}))),
                        "getHealth" => (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(json!({}))),
                        "getBlockHeight" => (axum::http::StatusCode::BAD_GATEWAY, Json(json!({}))),
                        "getBalance" => {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            (axum::http::StatusCode::OK, Json(json!({})))
                        }
                        _ => (
                            axum::http::StatusCode::OK,
                            Json(json!({"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"Transaction simulation failed"}})),
                        ),
                    }
                }
            }),
        );
        let url = serve(app).await;
        let mut cfg = RpcConfig::new(url);
        cfg.timeout = Duration::from_millis(150);
        let c = RpcClient::new(cfg).unwrap();
        assert_eq!(c.get_slot(Commitment::Processed).await.unwrap(), 42);
        let e = c.get_health().await.unwrap_err();
        assert!(e.is_rate_limited() && !e.is_ambiguous());
        let e = c.get_block_height(Commitment::Confirmed).await.unwrap_err();
        assert!(e.is_ambiguous());
        let e = c
            .get_balance(&Pubkey::new_unique(), Commitment::Confirmed)
            .await
            .unwrap_err();
        assert!(matches!(e, RpcError::Timeout(_)), "{e:?}");
        let e = c
            .send_transaction(&[1, 2, 3], false, Some(0), Commitment::Processed)
            .await
            .unwrap_err();
        assert!(matches!(e, RpcError::Rpc { code: -32002, .. }));
        assert!(!e.is_retryable());
        assert_eq!(hits.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn parses_transaction_balances() {
        let v = json!({
            "slot": 10, "blockTime": 1700000000,
            "transaction": {"message": {"accountKeys": ["A","B"]}},
            "meta": {
                "fee": 5000, "err": null, "computeUnitsConsumed": 1234,
                "preBalances": [100, 0], "postBalances": [50, 45],
                "loadedAddresses": {"writable": ["C"], "readonly": []},
                "preTokenBalances": [{"accountIndex": 1, "mint": "M", "owner": "A", "uiTokenAmount": {"amount": "10", "decimals": 6}}],
                "postTokenBalances": [{"accountIndex": 1, "mint": "M", "owner": "A", "uiTokenAmount": {"amount": "25", "decimals": 6}},
                                      {"accountIndex": 2, "mint": "M", "owner": "A", "uiTokenAmount": {"amount": "5", "decimals": 6}}],
                "logMessages": ["Program log: hi"]
            }
        });
        let t = parse_transaction(&v).unwrap();
        assert_eq!(t.account_keys, vec!["A", "B", "C"]);
        assert_eq!(t.lamport_delta("A"), Some(-50));
        assert_eq!(t.token_delta("A", "M"), 20);
        assert!(t.err.is_none());
        assert_eq!(t.compute_units_consumed, Some(1234));
    }

    #[test]
    fn redacts_query() {
        assert_eq!(
            redact("error sending request for url (https://x.io/?api-key=SECRET)"),
            "error sending request for url (https://x.io/?<redacted>)"
        );
        assert_eq!(
            url_host("https://mainnet.helius-rpc.com/?api-key=abc"),
            "mainnet.helius-rpc.com"
        );
    }
}
