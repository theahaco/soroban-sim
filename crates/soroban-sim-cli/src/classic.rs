//! Plan-file text for classic operations: amounts with seven decimal places,
//! `CODE:ISSUER` assets, account flag names.

use anyhow::{anyhow, bail, Result};
use soroban_env_host::xdr::{AccountId, AlphaNum12, AlphaNum4, Asset, AssetCode12, AssetCode4};

use crate::plan::FlagsSpec;

/// An amount in units of an asset (XLM or an issued asset), as stroops:
/// `5`, `2.5`, `0.0000001`. At most seven decimal places.
pub fn amount(text: &str) -> Result<i64> {
    let t = text.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let (whole, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && frac.is_empty() || !whole.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        bail!("{text:?} is not an amount");
    }
    if frac.len() > 7 {
        bail!("{text:?} has more than seven decimal places");
    }
    let whole: i64 = if whole.is_empty() { 0 } else { whole.parse().map_err(|_| anyhow!("{text:?} is too large"))? };
    let frac: i64 = format!("{frac:0<7}").parse()?;
    let stroops = whole.checked_mul(10_000_000).and_then(|w| w.checked_add(frac)).ok_or_else(|| anyhow!("{text:?} is too large"))?;
    Ok(if negative { -stroops } else { stroops })
}

/// `native` (or `XLM`), or `CODE:ISSUER` with a 1-12 character code.
pub fn asset(text: &str) -> Result<Asset> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("native") || t.eq_ignore_ascii_case("xlm") {
        return Ok(Asset::Native);
    }
    let (code, issuer) = t.split_once(':').ok_or_else(|| anyhow!("asset {t:?} is not `native` or CODE:ISSUER"))?;
    let issuer: AccountId = issuer.parse().map_err(|_| anyhow!("asset {t:?}: {issuer} is not a G address"))?;
    let bytes = code.as_bytes();
    Ok(match bytes.len() {
        1..=4 => {
            let mut c = [0u8; 4];
            c[..bytes.len()].copy_from_slice(bytes);
            Asset::CreditAlphanum4(AlphaNum4 { asset_code: AssetCode4(c), issuer })
        }
        5..=12 => {
            let mut c = [0u8; 12];
            c[..bytes.len()].copy_from_slice(bytes);
            Asset::CreditAlphanum12(AlphaNum12 { asset_code: AssetCode12(c), issuer })
        }
        _ => bail!("asset code {code:?} must have 1 to 12 characters"),
    })
}

/// Account flags by name or as bits.
pub fn flags(spec: &FlagsSpec) -> Result<u32> {
    Ok(match spec {
        FlagsSpec::Bits(b) => *b,
        FlagsSpec::Names(names) => {
            let mut bits = 0;
            for n in names {
                bits |= match n.as_str() {
                    "auth_required" => 1,
                    "auth_revocable" => 2,
                    "auth_immutable" => 4,
                    "auth_clawback_enabled" => 8,
                    other => bail!("unknown account flag {other:?} (auth_required, auth_revocable, auth_immutable, auth_clawback_enabled)"),
                };
            }
            bits
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_have_seven_decimal_places() {
        assert_eq!(amount("5").unwrap(), 50_000_000);
        assert_eq!(amount("2.5").unwrap(), 25_000_000);
        assert_eq!(amount("0.0000001").unwrap(), 1);
        assert_eq!(amount(".5").unwrap(), 5_000_000);
        assert_eq!(amount("-1").unwrap(), -10_000_000);
        assert!(amount("0.00000001").is_err());
        assert!(amount("1e3").is_err());
        assert!(amount("").is_err());
        assert!(amount("922337203686").is_err());
    }

    #[test]
    fn assets_parse_by_code_length() {
        let g = "GAMPJROHOAW662FINQ4XQOY2ULX5IEGYXCI4SMZYE75EHQBR6PSTJG3M";
        assert_eq!(asset("native").unwrap(), Asset::Native);
        assert!(matches!(asset(&format!("USD:{g}")).unwrap(), Asset::CreditAlphanum4(_)));
        assert!(matches!(asset(&format!("LONGCODE:{g}")).unwrap(), Asset::CreditAlphanum12(_)));
        assert!(asset("USD").is_err());
        assert!(asset(&format!("THIRTEENCHARS:{g}")).is_err());
    }
}
