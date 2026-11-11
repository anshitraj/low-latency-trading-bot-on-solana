//! Minimal Telegram Bot API client (https://core.telegram.org/bots/api,
//! accessed 2026-11-11). HTML parse mode; every dynamic string is escaped.
//! 429 responses carry `parameters.retry_after`; callers back off.

use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum TgError {
    #[error("rate limited, retry after {0}s")]
    RetryAfter(u64),
    #[error("telegram api error {code}: {description}")]
    Api { code: i64, description: String },
    #[error("transport error")]
    Transport,
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl TelegramClient {
    pub fn new(base: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .tcp_nodelay(true)
            .build()
            .expect("client");
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    async fn call(&self, method: &str, body: Value) -> Result<Value, TgError> {
        // URL contains the token: never log it (TgError carries no URL).
        let url = format!("{}/bot{}/{}", self.base, self.token, method);
        let resp = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|_| TgError::Transport)?;
        let v: Value = resp.json().await.map_err(|_| TgError::Transport)?;
        if v["ok"].as_bool() == Some(true) {
            return Ok(v["result"].clone());
        }
        if let Some(r) = v
            .pointer("/parameters/retry_after")
            .and_then(|r| r.as_u64())
        {
            return Err(TgError::RetryAfter(r));
        }
        Err(TgError::Api {
            code: v["error_code"].as_i64().unwrap_or(0),
            description: v["description"]
                .as_str()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect(),
        })
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        html: &str,
        markup: Option<Value>,
    ) -> Result<i64, TgError> {
        let mut b = json!({"chat_id": chat_id, "text": html, "parse_mode": "HTML", "link_preview_options": {"is_disabled": true}});
        if let Some(m) = markup {
            b["reply_markup"] = m;
        }
        let r = self.call("sendMessage", b).await?;
        Ok(r["message_id"].as_i64().unwrap_or(0))
    }

    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        html: &str,
        markup: Option<Value>,
    ) -> Result<(), TgError> {
        let mut b = json!({"chat_id": chat_id, "message_id": message_id, "text": html, "parse_mode": "HTML", "link_preview_options": {"is_disabled": true}});
        if let Some(m) = markup {
            b["reply_markup"] = m;
        }
        match self.call("editMessageText", b).await {
            Ok(_) => Ok(()),
            // Editing to identical content is not an error for us.
            Err(TgError::Api { description, .. })
                if description.contains("message is not modified") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<(), TgError> {
        self.call("answerCallbackQuery", json!({"callback_query_id": callback_id, "text": text.chars().take(190).collect::<String>()})).await.map(|_| ())
    }

    pub async fn set_webhook(&self, url: &str, secret: &str) -> Result<(), TgError> {
        self.call("setWebhook", json!({"url": url, "secret_token": secret, "allowed_updates": ["message", "callback_query"], "drop_pending_updates": false, "max_connections": 20})).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_html() {
        assert_eq!(esc("<b>&\"x\""), "&lt;b&gt;&amp;&quot;x&quot;");
    }
}
