//! Multi-provider RPC pool internals: per-endpoint client-side rate limiting
//! with a reserve for trade-critical calls, cooldown on 429 / transport errors,
//! and endpoint specs loaded from env / file.
//!
//! Rules:
//! - Each endpoint has a token bucket (default 8 req/s, burst 8). Background
//!   calls (status polls, getTransaction, leader schedule, block height) may not
//!   take the last `reserve` tokens; trade-critical calls (account reads,
//!   blockhash, simulate, send) may. So polling can never starve a trade.
//! - A 429 puts the endpoint in cooldown (250 ms doubling to 10 s) and the call
//!   moves to the next endpoint. Transport errors / timeouts / 5xx also move on
//!   (reads only). JSON-RPC errors are deterministic and returned as-is.
//! - `sendTransaction` never fails over automatically: the sender layer owns
//!   ambiguity classification for sends.

use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const DEFAULT_RPS: f64 = 8.0;
const RESERVE_TOKENS: f64 = 2.0;
const COOLDOWN_BASE: Duration = Duration::from_millis(250);
const COOLDOWN_MAX: Duration = Duration::from_secs(10);

/// Trade-critical methods may use the reserved tokens.
pub fn is_critical(method: &str) -> bool {
    matches!(
        method,
        "getMultipleAccounts"
            | "getAccountInfo"
            | "getLatestBlockhash"
            | "sendTransaction"
            | "simulateTransaction"
            | "getBalance"
            | "getMinimumBalanceForRentExemption"
            | "getGenesisHash"
    )
}

pub struct RateLimiter {
    rate: f64,
    burst: f64,
    st: Mutex<(f64, Instant)>,
}

impl RateLimiter {
    pub fn new(rps: f64) -> Self {
        let rate = rps.max(0.5);
        let burst = rate.max(RESERVE_TOKENS + 1.0);
        Self {
            rate,
            burst,
            st: Mutex::new((burst, Instant::now())),
        }
    }

    /// Take a token, or return how long until one is available for this class.
    pub fn try_take(&self, critical: bool) -> Result<(), Duration> {
        let mut g = self.st.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(g.1).as_secs_f64();
        g.0 = (g.0 + elapsed * self.rate).min(self.burst);
        g.1 = now;
        let floor = if critical { 0.0 } else { RESERVE_TOKENS };
        if g.0 - 1.0 >= floor {
            g.0 -= 1.0;
            Ok(())
        } else {
            let need = floor + 1.0 - g.0;
            Err(Duration::from_secs_f64(need / self.rate))
        }
    }
}

#[derive(Default)]
pub struct EndpointStats {
    pub ok: AtomicU64,
    pub rate_limited: AtomicU64,
    pub errors: AtomicU64,
}

pub struct Health {
    cooldown_until: Mutex<Option<Instant>>,
    strikes: AtomicU32,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            cooldown_until: Mutex::new(None),
            strikes: AtomicU32::new(0),
        }
    }
}

impl Health {
    pub fn cooling_for(&self) -> Option<Duration> {
        let g = self.cooldown_until.lock();
        g.and_then(|t| t.checked_duration_since(Instant::now()))
    }
    pub fn strike(&self) {
        let n = self.strikes.fetch_add(1, Ordering::Relaxed).min(6);
        let d = (COOLDOWN_BASE * 2u32.pow(n)).min(COOLDOWN_MAX);
        *self.cooldown_until.lock() = Some(Instant::now() + d);
    }
    pub fn ok(&self) {
        self.strikes.store(0, Ordering::Relaxed);
    }
}

/// One provider: HTTP URL, optional WebSocket URL, optional rate limit.
#[derive(Clone, Debug, PartialEq)]
pub struct EndpointSpec {
    pub http: String,
    pub ws: Option<String>,
    pub rps: Option<f64>,
}

impl EndpointSpec {
    pub fn ws_url(&self) -> String {
        self.ws
            .clone()
            .unwrap_or_else(|| crate::ws::ws_url_from_http(&self.http))
    }
}

/// Lines: `<http-url> [<ws-url>] [rps=<n>]`; blank lines and `#` comments ignored.
pub fn parse_endpoint_lines(text: &str) -> Vec<EndpointSpec> {
    let mut out = vec![];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut spec = EndpointSpec {
            http: String::new(),
            ws: None,
            rps: None,
        };
        for tok in line.split_whitespace() {
            if let Some(v) = tok.strip_prefix("rps=") {
                spec.rps = v.parse().ok().filter(|r: &f64| *r > 0.0);
            } else if tok.starts_with("ws://") || tok.starts_with("wss://") {
                spec.ws = Some(tok.to_string());
            } else if (tok.starts_with("http://") || tok.starts_with("https://"))
                && spec.http.is_empty()
            {
                spec.http = tok.to_string();
            }
        }
        if !spec.http.is_empty() && !out.iter().any(|s: &EndpointSpec| s.http == spec.http) {
            out.push(spec);
        }
    }
    out
}

/// `primary` (usually RPC_URL) first, then the lines of `RPC_URLS_FILE`
/// (default `~/.trader/rpc_urls`) if it exists. Duplicates removed.
pub fn load_endpoint_specs(primary: Option<&str>) -> Vec<EndpointSpec> {
    let mut text = String::new();
    if let Some(p) = primary {
        text.push_str(p);
        text.push('\n');
    }
    let file = std::env::var("RPC_URLS_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .map(|h| PathBuf::from(h).join(".trader").join("rpc_urls"))
        });
    if let Some(f) = file {
        if let Ok(t) = std::fs::read_to_string(&f) {
            text.push_str(&t);
        }
    }
    parse_endpoint_lines(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_with_ws_and_rps_and_dedupes() {
        let t = "# comment\nhttps://a.example/?k=1\n\nhttps://b.example/x wss://b-ws.example/x rps=20\nhttps://a.example/?k=1\nnot-a-url\n";
        let v = parse_endpoint_lines(t);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ws_url(), "wss://a.example/?k=1");
        assert_eq!(v[1].ws.as_deref(), Some("wss://b-ws.example/x"));
        assert_eq!(v[1].rps, Some(20.0));
    }

    #[test]
    fn limiter_reserves_tokens_for_critical_calls() {
        let l = RateLimiter::new(4.0); // burst 4, reserve 2
        assert!(l.try_take(false).is_ok());
        assert!(l.try_take(false).is_ok());
        // Only the reserve is left: background must wait, critical may proceed.
        assert!(l.try_take(false).is_err());
        assert!(l.try_take(true).is_ok());
        assert!(l.try_take(true).is_ok());
        assert!(l.try_take(true).is_err());
    }

    #[test]
    fn health_cooldown_grows_and_resets() {
        let h = Health::default();
        assert!(h.cooling_for().is_none());
        h.strike();
        let a = h.cooling_for().unwrap();
        h.strike();
        let b = h.cooling_for().unwrap();
        assert!(b > a);
        h.ok();
        assert_eq!(h.strikes.load(Ordering::Relaxed), 0);
    }
}
