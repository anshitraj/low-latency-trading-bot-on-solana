//! Exact integer money math. No floating point anywhere.

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
pub const SOL_DECIMALS: u8 = 9;
pub const BPS_DENOMINATOR: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("empty amount")]
    Empty,
    #[error("invalid amount format: {0}")]
    InvalidFormat(String),
    #[error("amount has more than {max} decimal places")]
    TooManyDecimals { max: u8 },
    #[error("amount overflows u64 base units")]
    Overflow,
    #[error("amount must be greater than zero")]
    Zero,
    #[error("basis points must be <= 10000, got {0}")]
    BpsOutOfRange(u32),
    #[error("arithmetic underflow")]
    Underflow,
}

/// Lamports (1e-9 SOL). Serialized as a decimal *string* in JSON so JavaScript
/// clients never round-trip it through IEEE-754 doubles.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Lamports(pub u64);

impl Lamports {
    pub const ZERO: Lamports = Lamports(0);

    pub fn from_sol_str(s: &str) -> Result<Self, MoneyError> {
        parse_units(s, SOL_DECIMALS).map(Lamports)
    }
    pub fn to_sol_string(self) -> String {
        format_units(self.0, SOL_DECIMALS)
    }
    pub fn checked_add(self, o: Lamports) -> Result<Lamports, MoneyError> {
        self.0.checked_add(o.0).map(Lamports).ok_or(MoneyError::Overflow)
    }
    pub fn checked_sub(self, o: Lamports) -> Result<Lamports, MoneyError> {
        self.0.checked_sub(o.0).map(Lamports).ok_or(MoneyError::Underflow)
    }
    pub fn saturating_sub(self, o: Lamports) -> Lamports {
        Lamports(self.0.saturating_sub(o.0))
    }
}

impl fmt::Display for Lamports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} SOL", self.to_sol_string())
    }
}

impl Serialize for Lamports {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Lamports {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        u64_string::deserialize(d).map(Lamports)
    }
}

/// serde helper: u64 as decimal string, accepting either a string or a JSON
/// integer on input (never a float).
pub mod u64_string {
    use super::*;
    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = u64;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a non-negative integer or decimal integer string")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
                Ok(v)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
                u64::try_from(v).map_err(|_| E::custom("negative amount"))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<u64, E> {
                Err(E::custom("floating point amounts are not accepted"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
                if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(E::custom("amount string must contain only digits"));
                }
                v.parse::<u64>().map_err(|_| E::custom("amount overflows u64"))
            }
        }
        d.deserialize_any(V)
    }
}

/// Raw token amount together with the mint's decimals.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAmount {
    #[serde(with = "u64_string")]
    pub raw: u64,
    pub decimals: u8,
}

impl TokenAmount {
    pub fn new(raw: u64, decimals: u8) -> Self {
        Self { raw, decimals }
    }
    pub fn ui_string(&self) -> String {
        format_units(self.raw, self.decimals)
    }
}

/// Basis points, validated to 0..=10_000.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct Bps(u16);

impl Bps {
    pub const MAX: Bps = Bps(10_000);
    pub fn new(v: u32) -> Result<Self, MoneyError> {
        if v > 10_000 {
            Err(MoneyError::BpsOutOfRange(v))
        } else {
            Ok(Bps(v as u16))
        }
    }
    pub fn get(self) -> u16 {
        self.0
    }
}
impl TryFrom<u32> for Bps {
    type Error = MoneyError;
    fn try_from(v: u32) -> Result<Self, Self::Error> {
        Bps::new(v)
    }
}
impl From<Bps> for u32 {
    fn from(b: Bps) -> u32 {
        b.0 as u32
    }
}

/// Parse a non-negative decimal string into base units. Rejects (rather than
/// rounds) inputs with more precision than `decimals`.
pub fn parse_units(s: &str, decimals: u8) -> Result<u64, MoneyError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(MoneyError::Empty);
    }
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(MoneyError::InvalidFormat(s.to_string()));
    }
    let digits_ok = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if !digits_ok(int_part) || !digits_ok(frac_part) || frac_part.contains('.') {
        return Err(MoneyError::InvalidFormat(s.to_string()));
    }
    if frac_part.len() > decimals as usize {
        // Allow trailing zeros beyond precision ("1.50000000000" for 9 decimals)
        let (keep, extra) = frac_part.split_at(decimals as usize);
        if !extra.bytes().all(|b| b == b'0') {
            return Err(MoneyError::TooManyDecimals { max: decimals });
        }
        return parse_units(&format!("{int_part}.{keep}"), decimals);
    }
    let scale = 10u128.pow(decimals as u32);
    let int_val: u128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse::<u128>().map_err(|_| MoneyError::Overflow)?
    };
    let mut frac_val: u128 = 0;
    if !frac_part.is_empty() {
        frac_val = frac_part.parse::<u128>().map_err(|_| MoneyError::Overflow)?;
        frac_val *= 10u128.pow((decimals as usize - frac_part.len()) as u32);
    }
    let total = int_val
        .checked_mul(scale)
        .and_then(|v| v.checked_add(frac_val))
        .ok_or(MoneyError::Overflow)?;
    u64::try_from(total).map_err(|_| MoneyError::Overflow)
}

/// Like [`parse_units`] but rejects zero.
pub fn parse_positive_units(s: &str, decimals: u8) -> Result<u64, MoneyError> {
    match parse_units(s, decimals)? {
        0 => Err(MoneyError::Zero),
        v => Ok(v),
    }
}

/// Format base units as a decimal string, trimming trailing zeros.
pub fn format_units(raw: u64, decimals: u8) -> String {
    if decimals == 0 {
        return raw.to_string();
    }
    let scale = 10u128.pow(decimals as u32);
    let raw = raw as u128;
    let int = raw / scale;
    let frac = raw % scale;
    if frac == 0 {
        return int.to_string();
    }
    let frac_s = format!("{:0width$}", frac, width = decimals as usize);
    format!("{}.{}", int, frac_s.trim_end_matches('0'))
}

/// Minimum acceptable output after slippage: floor(out * (10000 - bps) / 10000).
/// Rounds *down* so the threshold is never looser than requested.
pub fn min_out_after_slippage(expected_out: u64, slippage: Bps) -> u64 {
    let num = expected_out as u128 * (BPS_DENOMINATOR - slippage.0 as u64) as u128;
    (num / BPS_DENOMINATOR as u128) as u64
}

/// Maximum acceptable input after slippage: ceil(in * (10000 + bps) / 10000),
/// saturating at u64::MAX. Rounds *up* by at most one base unit.
pub fn max_in_after_slippage(expected_in: u64, slippage: Bps) -> u64 {
    let num = expected_in as u128 * (BPS_DENOMINATOR + slippage.0 as u64) as u128;
    let v = num.div_ceil(BPS_DENOMINATOR as u128);
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// floor(balance * bps / 10000); exactly `balance` when bps == 10000.
pub fn portion_bps(balance: u64, bps: Bps) -> u64 {
    if bps.0 == 10_000 {
        return balance;
    }
    ((balance as u128 * bps.0 as u128) / BPS_DENOMINATOR as u128) as u64
}

/// Price impact in bps between a spot output (no-size) and an actual output.
/// Returns None when spot_out is zero.
pub fn price_impact_bps(spot_out: u128, actual_out: u128) -> Option<u32> {
    if spot_out == 0 {
        return None;
    }
    if actual_out >= spot_out {
        return Some(0);
    }
    let diff = spot_out - actual_out;
    Some(((diff * 10_000) / spot_out).min(10_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parse_sol_exact() {
        assert_eq!(Lamports::from_sol_str("0.05").unwrap().0, 50_000_000);
        assert_eq!(Lamports::from_sol_str("1").unwrap().0, 1_000_000_000);
        assert_eq!(Lamports::from_sol_str(".5").unwrap().0, 500_000_000);
        assert_eq!(Lamports::from_sol_str("0.000000001").unwrap().0, 1);
        assert_eq!(Lamports::from_sol_str("1.500000000000").unwrap().0, 1_500_000_000);
        assert_eq!(Lamports::from_sol_str("18446744073.709551615").unwrap().0, u64::MAX);
    }

    #[test]
    fn parse_rejects_bad_input() {
        for bad in ["", " ", ".", "-1", "+1", "1e9", "1.2.3", "abc", "0x10", "1,5", "NaN", "inf"] {
            assert!(Lamports::from_sol_str(bad).is_err(), "should reject {bad:?}");
        }
        assert_eq!(
            Lamports::from_sol_str("0.0000000001"),
            Err(MoneyError::TooManyDecimals { max: 9 })
        );
        assert_eq!(Lamports::from_sol_str("18446744073.709551616"), Err(MoneyError::Overflow));
        assert_eq!(parse_positive_units("0.000", 9), Err(MoneyError::Zero));
    }

    #[test]
    fn format_roundtrip_examples() {
        assert_eq!(format_units(50_000_000, 9), "0.05");
        assert_eq!(format_units(0, 9), "0");
        assert_eq!(format_units(1, 6), "0.000001");
        assert_eq!(format_units(123_456_789, 0), "123456789");
        assert_eq!(format_units(u64::MAX, 9), "18446744073.709551615");
    }

    #[test]
    fn slippage_rounding_direction() {
        let s = Bps::new(100).unwrap();
        assert_eq!(min_out_after_slippage(1000, s), 990);
        assert_eq!(min_out_after_slippage(999, s), 989); // 989.01 floors
        assert_eq!(max_in_after_slippage(1000, s), 1010);
        assert_eq!(max_in_after_slippage(999, s), 1009); // 1008.99 ceils
        assert_eq!(min_out_after_slippage(u64::MAX, Bps::new(0).unwrap()), u64::MAX);
        assert_eq!(max_in_after_slippage(u64::MAX, s), u64::MAX);
        assert_eq!(min_out_after_slippage(1000, Bps::MAX), 0);
    }

    #[test]
    fn bps_validation() {
        assert!(Bps::new(10_001).is_err());
        let b: Result<Bps, _> = serde_json::from_str("10001");
        assert!(b.is_err());
    }

    #[test]
    fn portion_exact_full() {
        assert_eq!(portion_bps(12345, Bps::MAX), 12345);
        assert_eq!(portion_bps(12345, Bps::new(5000).unwrap()), 6172);
        assert_eq!(portion_bps(u64::MAX, Bps::new(2500).unwrap()), u64::MAX / 4);
    }

    #[test]
    fn lamports_json_is_string_and_rejects_float() {
        let j = serde_json::to_string(&Lamports(42)).unwrap();
        assert_eq!(j, "\"42\"");
        assert_eq!(serde_json::from_str::<Lamports>("\"42\"").unwrap(), Lamports(42));
        assert_eq!(serde_json::from_str::<Lamports>("42").unwrap(), Lamports(42));
        assert!(serde_json::from_str::<Lamports>("42.0").is_err());
        assert!(serde_json::from_str::<Lamports>("-1").is_err());
        assert!(serde_json::from_str::<Lamports>("\"1e3\"").is_err());
    }

    #[test]
    fn impact() {
        assert_eq!(price_impact_bps(1000, 990), Some(100));
        assert_eq!(price_impact_bps(0, 0), None);
        assert_eq!(price_impact_bps(1000, 1001), Some(0));
    }

    proptest! {
        #[test]
        fn prop_format_parse_roundtrip(raw in any::<u64>(), dec in 0u8..=12) {
            let s = format_units(raw, dec);
            prop_assert_eq!(parse_units(&s, dec).unwrap(), raw);
        }

        #[test]
        fn prop_min_out_never_exceeds_expected(out in any::<u64>(), bps in 0u32..=10_000) {
            let b = Bps::new(bps).unwrap();
            prop_assert!(min_out_after_slippage(out, b) <= out);
            prop_assert!(max_in_after_slippage(out, b) >= out);
        }

        #[test]
        fn prop_portion_le_balance(bal in any::<u64>(), bps in 0u32..=10_000) {
            prop_assert!(portion_bps(bal, Bps::new(bps).unwrap()) <= bal);
        }
    }
}
