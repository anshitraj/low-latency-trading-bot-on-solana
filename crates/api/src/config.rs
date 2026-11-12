//! Configuration from environment. Safe defaults: mock mode, mainnet trading
//! off, no external providers. See .env.example for every variable.

use std::str::FromStr;
use trader_domain::Network;

/// Mainnet order submission requires this exact value in MAINNET_TRADING_ENABLED.
pub const MAINNET_ACK: &str = "I_UNDERSTAND_REAL_FUNDS_AT_RISK";

#[derive(Clone)]
pub struct Config {
    pub mode: Network,
    pub bind: String,
    pub rpc_url: Option<String>,
    /// WebSocket RPC URL; default derived from RPC_URL (http->ws, https->wss).
    pub ws_url: Option<String>,
    /// Stream coin state + wallet balances over WebSocket (default on outside mock).
    pub ws_streaming: bool,
    pub database_url: Option<String>,
    pub bot_token: Option<String>,
    pub webhook_secret: Option<String>,
    pub telegram_api_base: String,
    pub public_url: Option<String>,
    pub miniapp_dir: Option<String>,
    pub cors_origin: Option<String>,
    pub kek: Option<Vec<u8>>,
    pub kek_id: String,
    pub admin_token: Option<String>,
    pub mainnet_trading_enabled: bool,
    pub beta_allowlist: Vec<i64>,
    pub initdata_max_age_secs: i64,
    pub session_ttl_secs: i64,
    pub jito_url: Option<String>,
    pub jito_bundle_only: bool,
    pub jito_rps: u32,
    pub helius_sender_url: Option<String>,
    pub helius_swqos_only: bool,
    pub skip_preflight: bool,
    pub observer_poll_ms: u64,
    /// MOCK chain slot time (ms). Default 400 like mainnet; tests use ~20.
    pub mock_slot_ms: u64,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print secrets.
        f.debug_struct("Config")
            .field("mode", &self.mode)
            .field("bind", &self.bind)
            .field(
                "rpc_host",
                &self
                    .rpc_url
                    .as_deref()
                    .map(trader_market_data::rpc::url_host),
            )
            .field("database", &self.database_url.is_some())
            .field("ws_streaming", &self.ws_streaming)
            .field("telegram", &self.bot_token.is_some())
            .field("webhook_secret", &self.webhook_secret.is_some())
            .field("kek", &self.kek.is_some())
            .field("mainnet_trading_enabled", &self.mainnet_trading_enabled)
            .field("jito", &self.jito_url.is_some())
            .field("helius_sender", &self.helius_sender_url.is_some())
            .finish()
    }
}

fn env(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let mode = Network::from_str(&env("TRADER_MODE").unwrap_or_else(|| "mock".into()))
            .map_err(anyhow::Error::msg)?;
        let kek = match env("KEK_BASE64") {
            Some(b) => {
                let k = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b)?;
                anyhow::ensure!(k.len() == 32, "KEK_BASE64 must decode to 32 bytes");
                Some(k)
            }
            None => None,
        };
        let mainnet_trading_enabled =
            env("MAINNET_TRADING_ENABLED").as_deref() == Some(MAINNET_ACK);
        let cfg = Self {
            mode,
            bind: env("BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into()),
            rpc_url: env("RPC_URL"),
            ws_url: env("WS_URL"),
            ws_streaming: env("WS_STREAMING").as_deref() != Some("false"),
            database_url: env("DATABASE_URL"),
            bot_token: env("TELEGRAM_BOT_TOKEN"),
            webhook_secret: env("TELEGRAM_WEBHOOK_SECRET"),
            telegram_api_base: env("TELEGRAM_API_BASE")
                .unwrap_or_else(|| "https://api.telegram.org".into()),
            public_url: env("PUBLIC_URL"),
            miniapp_dir: env("MINIAPP_DIR"),
            cors_origin: env("CORS_ORIGIN"),
            kek,
            kek_id: env("KEK_ID").unwrap_or_else(|| "local-env-1".into()),
            admin_token: env("ADMIN_TOKEN"),
            mainnet_trading_enabled,
            beta_allowlist: env("BETA_ALLOWLIST_TELEGRAM_IDS")
                .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
                .unwrap_or_default(),
            initdata_max_age_secs: env("INITDATA_MAX_AGE_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
            session_ttl_secs: env("SESSION_TTL_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1800),
            jito_url: env("JITO_BLOCK_ENGINE_URL"),
            jito_bundle_only: env("JITO_BUNDLE_ONLY").as_deref() == Some("true"),
            jito_rps: env("JITO_RPS").and_then(|s| s.parse().ok()).unwrap_or(1),
            helius_sender_url: env("HELIUS_SENDER_URL"),
            helius_swqos_only: env("HELIUS_SWQOS_ONLY").as_deref() != Some("false"),
            skip_preflight: env("SKIP_PREFLIGHT").as_deref() == Some("true"),
            observer_poll_ms: env("OBSERVER_POLL_MS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(250),
            mock_slot_ms: env("MOCK_SLOT_MS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(400),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.mode != Network::Mock {
            anyhow::ensure!(
                self.rpc_url.is_some(),
                "RPC_URL is required outside mock mode"
            );
            anyhow::ensure!(
                self.kek.is_some(),
                "KEK_BASE64 is required outside mock mode (managed wallet encryption)"
            );
        }
        if self.mode == Network::Mainnet {
            anyhow::ensure!(
                self.database_url.is_some(),
                "DATABASE_URL is required on mainnet"
            );
            anyhow::ensure!(
                !self.beta_allowlist.is_empty(),
                "BETA_ALLOWLIST_TELEGRAM_IDS is required on mainnet (private beta)"
            );
        }
        if self.skip_preflight {
            anyhow::ensure!(
                self.mode != Network::Mainnet,
                "SKIP_PREFLIGHT is not permitted on mainnet in this build"
            );
        }
        if self.bot_token.is_some() {
            anyhow::ensure!(
                self.webhook_secret
                    .as_deref()
                    .map(|s| s.len() >= 24)
                    .unwrap_or(false),
                "TELEGRAM_WEBHOOK_SECRET (>=24 chars) is required with TELEGRAM_BOT_TOKEN"
            );
        }
        Ok(())
    }

    pub fn mock() -> Self {
        Self {
            mode: Network::Mock,
            bind: "127.0.0.1:0".into(),
            rpc_url: None,
            ws_url: None,
            ws_streaming: false,
            database_url: None,
            bot_token: None,
            webhook_secret: None,
            telegram_api_base: "https://api.telegram.org".into(),
            public_url: None,
            miniapp_dir: None,
            cors_origin: None,
            kek: None,
            kek_id: "mock".into(),
            admin_token: Some("test-admin-token".into()),
            mainnet_trading_enabled: false,
            beta_allowlist: vec![],
            initdata_max_age_secs: 300,
            session_ttl_secs: 1800,
            jito_url: None,
            jito_bundle_only: false,
            jito_rps: 1,
            helius_sender_url: None,
            helius_swqos_only: true,
            skip_preflight: false,
            observer_poll_ms: 20,
            mock_slot_ms: 20,
        }
    }

    pub fn beta_allowed(&self, telegram_id: i64) -> bool {
        self.beta_allowlist.is_empty() || self.beta_allowlist.contains(&telegram_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation_gates() {
        let mut c = Config::mock();
        assert!(c.validate().is_ok());
        c.mode = Network::Mainnet;
        assert!(c.validate().is_err(), "mainnet needs rpc/kek/db/allowlist");
        c.rpc_url = Some("https://x".into());
        c.kek = Some(vec![0; 32]);
        c.database_url = Some("postgres://x".into());
        assert!(c.validate().is_err(), "allowlist required");
        c.beta_allowlist = vec![1];
        assert!(c.validate().is_ok());
        c.skip_preflight = true;
        assert!(c.validate().is_err());
        let mut t = Config::mock();
        t.bot_token = Some("1:x".into());
        assert!(t.validate().is_err(), "webhook secret required");
        t.webhook_secret = Some("s".repeat(32));
        assert!(t.validate().is_ok());
        assert!(
            !format!("{t:?}").contains("sss"),
            "debug never prints secrets"
        );
    }
}
