//! Metrics registry rendered in Prometheus text format, plus redaction.
//! Kept dependency-free on purpose; the /metrics endpoint is private.

use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;

/// Microsecond buckets spanning 50 µs .. 60 s.
pub const US_BUCKETS: [u64; 18] = [
    50, 100, 250, 500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000,
    1_000_000, 2_500_000, 5_000_000, 15_000_000, 60_000_000,
];

#[derive(Default)]
struct Hist {
    buckets: Vec<u64>,
    sum: u64,
    count: u64,
}

#[derive(Default)]
struct Inner {
    counters: BTreeMap<String, u64>,
    hists: BTreeMap<String, Hist>,
    gauges: BTreeMap<String, i64>,
}

#[derive(Clone, Default)]
pub struct Metrics {
    inner: Arc<Mutex<Inner>>,
}

fn key(name: &str, labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return name.to_string();
    }
    let l: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", v.replace('"', "'")))
        .collect();
    format!("{name}{{{}}}", l.join(","))
}

impl Metrics {
    pub fn inc(&self, name: &str, labels: &[(&str, &str)]) {
        *self
            .inner
            .lock()
            .counters
            .entry(key(name, labels))
            .or_default() += 1;
    }
    pub fn gauge(&self, name: &str, labels: &[(&str, &str)], v: i64) {
        self.inner.lock().gauges.insert(key(name, labels), v);
    }
    pub fn observe_us(&self, name: &str, labels: &[(&str, &str)], us: u64) {
        let mut g = self.inner.lock();
        let h = g.hists.entry(key(name, labels)).or_insert_with(|| Hist {
            buckets: vec![0; US_BUCKETS.len()],
            ..Default::default()
        });
        for (i, b) in US_BUCKETS.iter().enumerate() {
            if us <= *b {
                h.buckets[i] += 1;
            }
        }
        h.sum += us;
        h.count += 1;
    }
    pub fn render(&self) -> String {
        let g = self.inner.lock();
        let mut out = String::new();
        for (k, v) in &g.counters {
            let _ = writeln!(out, "{k} {v}");
        }
        for (k, v) in &g.gauges {
            let _ = writeln!(out, "{k} {v}");
        }
        for (k, h) in &g.hists {
            let (name, labels) = match k.find('{') {
                Some(i) => (&k[..i], k[i + 1..k.len() - 1].to_string()),
                None => (k.as_str(), String::new()),
            };
            let sep = if labels.is_empty() { "" } else { "," };
            for (i, b) in US_BUCKETS.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {}",
                    h.buckets[i]
                );
            }
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}", h.count);
            let _ = writeln!(out, "{name}_sum{{{labels}}} {}", h.sum);
            let _ = writeln!(out, "{name}_count{{{labels}}} {}", h.count);
        }
        out
    }
}

/// Redact secrets that might appear in error strings or URLs.
pub fn redact(s: &str) -> String {
    const MARKERS: [&str; 5] = ["api-key=", "api_key=", "token=", "uuid=", "/bot"];
    const STOP: [char; 6] = ['&', '/', ' ', '"', ')', '?'];
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let bytes = s.as_bytes();
    'outer: while i < s.len() {
        for m in MARKERS {
            if bytes[i..].starts_with(m.as_bytes()) {
                out.push_str(m);
                let start = i + m.len();
                let end = s[start..].find(STOP).map(|e| start + e).unwrap_or(s.len());
                if end > start {
                    out.push_str("<redacted>");
                }
                i = end;
                continue 'outer;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn render_and_redact() {
        let m = Metrics::default();
        m.inc("orders_total", &[("state", "FINALIZED")]);
        m.observe_us("intent_to_signed_us", &[], 300);
        let r = m.render();
        assert!(r.contains("orders_total{state=\"FINALIZED\"} 1"));
        assert!(r.contains("intent_to_signed_us_bucket{le=\"500\"} 1"));
        assert!(r.contains("intent_to_signed_us_count{} 1"));
        assert_eq!(
            redact("https://x/?api-key=SECRET&y=1"),
            "https://x/?api-key=<redacted>&y=1"
        );
        assert_eq!(
            redact("https://api.telegram.org/bot123:ABC/sendMessage"),
            "https://api.telegram.org/bot<redacted>/sendMessage"
        );
        assert_eq!(redact("no secrets"), "no secrets");
    }
}
