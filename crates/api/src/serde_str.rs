//! Serde helpers that serialize integer types as JSON strings.
//!
//! Used by W5g execution DTOs so large u64 values (slots, lamports,
//! raw token amounts, transaction byte counts) survive a round-trip
//! through JavaScript without losing precision. JS `Number` is an
//! IEEE 754 double-precision float; only integers up to `2^53 − 1`
//! survive `JSON.parse` unchanged. Solana mainnet slot numbers
//! already exceed `2^29` and token raw amounts are u64, so a JS
//! consumer that expects a number can silently truncate.
//!
//! See [`docs.save.finance`] for an example of an API that
//! consistently strings u64 fields.
//!
//! Each sub-module exposes a `(serialize, deserialize)` pair that
//! plugs into a struct field via `#[serde(with = "crate::serde_str::…")]`.
//! The `Option<T>` flavours pass through `None` as JSON `null` and
//! decode any of `null` / missing field / JSON string back to `None`
//! / `Some(value)`.

#![allow(missing_docs)]

pub mod u64_string {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &u64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(D::Error::custom)
    }
}

pub mod i64_string {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(D::Error::custom)
    }
}

pub mod opt_u64_string {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => s.serialize_str(&n.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(s) => s.parse().map(Some).map_err(D::Error::custom),
        }
    }
}

pub mod opt_i64_string {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<i64>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(n) => s.serialize_str(&n.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        let opt = Option::<String>::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(s) => s.parse().map(Some).map_err(D::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Holder {
        #[serde(with = "u64_string")]
        a: u64,
        #[serde(default, with = "opt_u64_string")]
        b: Option<u64>,
        #[serde(default, with = "opt_i64_string")]
        c: Option<i64>,
    }

    #[test]
    fn u64_string_round_trip_includes_solana_scale_value() {
        // Slot value far above 2^32, still well below u64::MAX.
        let original = Holder {
            a: 419_048_388_u64,
            b: Some(9_007_199_254_740_993_u64), // 2^53 + 1 — would lose precision as JS number
            c: Some(-250_000_i64),
        };
        let json = serde_json::to_string(&original).unwrap();
        // Every field is a JSON string, NOT a number.
        assert!(json.contains(r#""a":"419048388""#));
        assert!(json.contains(r#""b":"9007199254740993""#));
        assert!(json.contains(r#""c":"-250000""#));
        let back: Holder = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn opt_fields_omit_or_null_decode_to_none() {
        let json = r#"{"a":"42"}"#;
        let h: Holder = serde_json::from_str(json).unwrap();
        assert_eq!(h.a, 42);
        assert!(h.b.is_none());
        assert!(h.c.is_none());
        let json = r#"{"a":"42","b":null,"c":null}"#;
        let h: Holder = serde_json::from_str(json).unwrap();
        assert!(h.b.is_none());
        assert!(h.c.is_none());
    }

    #[test]
    fn malformed_numeric_string_fails_deserialize() {
        let json = r#"{"a":"not-a-number"}"#;
        let r: Result<Holder, _> = serde_json::from_str(json);
        assert!(r.is_err());
    }
}
