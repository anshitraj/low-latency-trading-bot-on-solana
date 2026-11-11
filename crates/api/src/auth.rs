//! Authentication:
//! - Telegram Mini App `initData` validated server-side per
//!   https://core.telegram.org/bots/webapps (accessed 2026-11-11):
//!   secret_key = HMAC_SHA256(key="WebAppData", msg=bot_token)
//!   hash = hex(HMAC_SHA256(key=secret_key, msg=data_check_string)),
//!   data_check_string = all fields except `hash`, sorted, "k=v" joined by '\n'.
//!   `initDataUnsafe` is never accepted. `auth_date` must be recent, and each
//!   signed payload can mint at most one session (replay mitigation).
//! - Sessions: opaque random 32-byte bearer tokens; only SHA-256(token) is
//!   stored. Bearer (not cookie) auth means browsers never attach it
//!   implicitly, so CSRF does not apply; CORS is restricted to the Mini App origin.
//! - Telegram webhook: `X-Telegram-Bot-Api-Secret-Token` constant-time compare.

use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InitDataError {
    #[error("initData missing hash")]
    MissingHash,
    #[error("initData hash mismatch")]
    BadHash,
    #[error("initData missing/invalid auth_date")]
    BadAuthDate,
    #[error("initData expired")]
    Expired,
    #[error("initData auth_date is in the future")]
    FromFuture,
    #[error("initData missing user")]
    MissingUser,
    #[error("initData malformed")]
    Malformed,
    #[error("initData too large")]
    TooLarge,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TgUser {
    pub id: i64,
    pub username: Option<String>,
    pub first_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedInitData {
    pub user: TgUser,
    pub auth_date: i64,
    pub hash: String,
    pub start_param: Option<String>,
}

pub fn webapp_secret(bot_token: &str) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(b"WebAppData").expect("hmac");
    mac.update(bot_token.as_bytes());
    mac.finalize().into_bytes().into()
}

pub fn data_check_string(pairs: &[(String, String)]) -> String {
    let mut v: Vec<&(String, String)> = pairs.iter().filter(|(k, _)| k != "hash").collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.iter().map(|(k, val)| format!("{k}={val}")).collect::<Vec<_>>().join("\n")
}

pub fn sign_init_data(bot_token: &str, pairs: &[(String, String)]) -> String {
    let mut mac = HmacSha256::new_from_slice(&webapp_secret(bot_token)).expect("hmac");
    mac.update(data_check_string(pairs).as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_init_data(init_data: &str, bot_token: &str, now_unix: i64, max_age_secs: i64) -> Result<VerifiedInitData, InitDataError> {
    if init_data.len() > 8 * 1024 {
        return Err(InitDataError::TooLarge);
    }
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(init_data.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut seen = std::collections::HashSet::new();
    for (k, _) in &pairs {
        if !seen.insert(k.clone()) {
            return Err(InitDataError::Malformed); // duplicate keys are ambiguous
        }
    }
    let hash = pairs.iter().find(|(k, _)| k == "hash").map(|(_, v)| v.clone()).ok_or(InitDataError::MissingHash)?;
    let expected = sign_init_data(bot_token, &pairs);
    if expected.as_bytes().ct_eq(hash.to_ascii_lowercase().as_bytes()).unwrap_u8() != 1 {
        return Err(InitDataError::BadHash);
    }
    let auth_date: i64 = pairs.iter().find(|(k, _)| k == "auth_date").and_then(|(_, v)| v.parse().ok()).ok_or(InitDataError::BadAuthDate)?;
    if auth_date > now_unix + 60 {
        return Err(InitDataError::FromFuture);
    }
    if now_unix - auth_date > max_age_secs {
        return Err(InitDataError::Expired);
    }
    let user_json = pairs.iter().find(|(k, _)| k == "user").map(|(_, v)| v.clone()).ok_or(InitDataError::MissingUser)?;
    let user: TgUser = serde_json::from_str(&user_json).map_err(|_| InitDataError::Malformed)?;
    let start_param = pairs.iter().find(|(k, _)| k == "start_param").map(|(_, v)| v.clone());
    Ok(VerifiedInitData { user, auth_date, hash, start_param })
}

pub fn secret_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).unwrap_u8() == 1
}

pub fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b)
}

/// Short random id for Telegram callback_data (64-byte limit).
pub fn new_callback_token() -> String {
    let mut b = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut b);
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b)
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "123456:TEST-TOKEN-not-real";

    fn build(pairs: &[(&str, &str)]) -> String {
        let owned: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let hash = sign_init_data(BOT, &owned);
        let mut s = url::form_urlencoded::Serializer::new(String::new());
        for (k, v) in pairs {
            s.append_pair(k, v);
        }
        s.append_pair("hash", &hash);
        s.finish()
    }

    #[test]
    fn valid_init_data() {
        let now = 1_800_000_000;
        let d = build(&[("auth_date", &now.to_string()), ("query_id", "AAH"), ("user", r#"{"id":42,"first_name":"A","username":"alice"}"#), ("start_param", "So11111111111111111111111111111111111111112")]);
        let v = verify_init_data(&d, BOT, now + 10, 300).unwrap();
        assert_eq!(v.user.id, 42);
        assert_eq!(v.start_param.as_deref(), Some("So11111111111111111111111111111111111111112"));
    }

    #[test]
    fn rejects_forgery_tamper_expiry_and_unsafe() {
        let now = 1_800_000_000;
        let good = build(&[("auth_date", &now.to_string()), ("user", r#"{"id":42}"#)]);
        // wrong bot token
        assert_eq!(verify_init_data(&good, "999:OTHER", now, 300), Err(InitDataError::BadHash));
        // modified user id (impersonation attempt)
        let tampered = good.replace("%22id%22%3A42", "%22id%22%3A43");
        assert_ne!(tampered, good);
        assert_eq!(verify_init_data(&tampered, BOT, now, 300), Err(InitDataError::BadHash));
        // expired
        assert_eq!(verify_init_data(&good, BOT, now + 301, 300), Err(InitDataError::Expired));
        // future
        let fut = build(&[("auth_date", &(now + 1000).to_string()), ("user", r#"{"id":42}"#)]);
        assert_eq!(verify_init_data(&fut, BOT, now, 300), Err(InitDataError::FromFuture));
        // initDataUnsafe-style payload without hash
        assert_eq!(verify_init_data("user=%7B%22id%22%3A1%7D&auth_date=1", BOT, now, 300), Err(InitDataError::MissingHash));
        // missing user
        let nouser = build(&[("auth_date", &now.to_string())]);
        assert_eq!(verify_init_data(&nouser, BOT, now, 300), Err(InitDataError::MissingUser));
        // duplicate key smuggling
        let dup = format!("{good}&user=%7B%22id%22%3A1%7D");
        assert_eq!(verify_init_data(&dup, BOT, now, 300), Err(InitDataError::Malformed));
        // oversized
        assert_eq!(verify_init_data(&"a".repeat(9000), BOT, now, 300), Err(InitDataError::TooLarge));
    }

    #[test]
    fn secret_compare_and_tokens() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
        let t = new_token();
        assert!(t.len() >= 43);
        assert_ne!(t, new_token());
        assert_eq!(token_hash("x").len(), 64);
        assert!(new_callback_token().len() <= 20);
    }
}
