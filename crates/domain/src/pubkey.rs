//! Base58 pubkey helpers for JSON (solana-pubkey's serde impl is a byte array).

use serde::{Deserialize, Deserializer, Serializer};
pub use solana_pubkey::Pubkey;
use std::str::FromStr;

pub fn parse(s: &str) -> Result<Pubkey, String> {
    let s = s.trim();
    if s.len() < 32 || s.len() > 44 {
        return Err("invalid address length".into());
    }
    Pubkey::from_str(s).map_err(|_| "invalid base58 address".to_string())
}

pub mod b58 {
    use super::*;
    pub fn serialize<S: Serializer>(p: &Pubkey, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&p.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Pubkey, D::Error> {
        let s = String::deserialize(d)?;
        parse(&s).map_err(serde::de::Error::custom)
    }
}

pub mod b58_opt {
    use super::*;
    pub fn serialize<S: Serializer>(p: &Option<Pubkey>, s: S) -> Result<S::Ok, S::Error> {
        match p {
            Some(p) => s.serialize_some(&p.to_string()),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Pubkey>, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        s.map(|s| parse(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_validates() {
        assert!(parse("So11111111111111111111111111111111111111112").is_ok());
        assert!(parse("not-a-key").is_err());
        assert!(parse("0OIl0OIl0OIl0OIl0OIl0OIl0OIl0OIl").is_err());
        assert!(parse("").is_err());
    }
}
