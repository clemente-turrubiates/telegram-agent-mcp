//! Lenient parsing for numeric ID fields (`chat_id`, `reply_to_message_id`,
//! `after_seq`) coming in over MCP.
//!
//! Some MCP clients — particularly ones built on JavaScript, where every
//! number is an IEEE-754 double under the hood — hand large or negative
//! integers to tool calls in forms `serde`'s strict integer deserializer
//! rejects outright: as a JSON number written by a model in scientific
//! notation, or as a quoted numeric string (`"-5309690856"` instead of
//! `-5309690856`). Telegram chat IDs are exactly this kind of value: large,
//! frequently negative, and produced by whatever LLM is driving the client.
//!
//! Rather than let a formatting quirk 400 the whole tool call, these
//! deserializers accept a plain integer, an integral float, or a numeric
//! string, and the paired `schema_with` functions advertise that leniency in
//! the tool's JSON schema — otherwise a client that validates arguments
//! against the schema before sending would reject a string up front and
//! never even reach this code.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

fn parse_i64(value: &Value) -> Result<i64, String> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64))
            .ok_or_else(|| format!("expected an integer, got {n}")),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .or_else(|_| {
                s.trim()
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.fract() == 0.0)
                    .map(|f| f as i64)
                    .ok_or(())
            })
            .map_err(|_| format!("expected an integer string, got {s:?}")),
        other => Err(format!(
            "expected an integer or numeric string, got {other}"
        )),
    }
}

fn parse_u64(value: &Value) -> Result<u64, String> {
    match value {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                    .map(|f| f as u64)
            })
            .ok_or_else(|| format!("expected a non-negative integer, got {n}")),
        Value::String(s) => s
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("expected a non-negative integer string, got {s:?}")),
        other => Err(format!(
            "expected a non-negative integer or numeric string, got {other}"
        )),
    }
}

pub fn deserialize_i64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    parse_i64(&value).map_err(D::Error::custom)
}

pub fn deserialize_opt_i64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse_i64(&v).map(Some).map_err(D::Error::custom),
    }
}

pub fn deserialize_opt_u64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => Ok(None),
        Some(v) => parse_u64(&v).map(Some).map_err(D::Error::custom),
    }
}

pub fn i64_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["integer", "string"],
        "description": "A 64-bit integer. Some clients cannot represent large or negative numbers exactly as JSON numbers — pass it as a numeric string (e.g. \"-5309690856\") if that's how you have it."
    })
}

pub fn u64_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": ["integer", "string"],
        "minimum": 0,
        "description": "A non-negative 64-bit integer. May be given as a numeric string if your client can't represent it exactly as a number."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_json_i64(json: &str) -> Result<i64, String> {
        parse_i64(&serde_json::from_str::<Value>(json).unwrap())
    }

    fn from_json_u64(json: &str) -> Result<u64, String> {
        parse_u64(&serde_json::from_str::<Value>(json).unwrap())
    }

    #[test]
    fn accepts_a_plain_integer() {
        assert_eq!(from_json_i64("-5309690856"), Ok(-5309690856));
    }

    #[test]
    fn accepts_an_integral_float() {
        // What a JS client sends for a whole-number `Number` is still just
        // an ordinary JSON integer literal, but some serializers emit an
        // explicit fractional/exponent form for values that came from float
        // arithmetic; either way the value is integral and should parse.
        assert_eq!(from_json_i64("-5309690856.0"), Ok(-5309690856));
        assert_eq!(from_json_i64("-5.309690856e9"), Ok(-5309690856));
    }

    #[test]
    fn rejects_a_non_integral_float() {
        assert!(from_json_i64("1.5").is_err());
    }

    #[test]
    fn accepts_a_numeric_string() {
        assert_eq!(from_json_i64("\"-5309690856\""), Ok(-5309690856));
        // A quoted scientific-notation string, e.g. a model that formatted
        // the number as text rather than a JSON literal.
        assert_eq!(from_json_i64("\"-5.309690856e+09\""), Ok(-5309690856));
    }

    #[test]
    fn rejects_garbage_strings() {
        assert!(from_json_i64("\"not a number\"").is_err());
    }

    #[test]
    fn u64_rejects_negative_values() {
        assert!(from_json_u64("-1").is_err());
        assert!(from_json_u64("\"-1\"").is_err());
    }

    #[test]
    fn u64_accepts_plain_and_string_forms() {
        assert_eq!(from_json_u64("42"), Ok(42));
        assert_eq!(from_json_u64("\"42\""), Ok(42));
    }
}
