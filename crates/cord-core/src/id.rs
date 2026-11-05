//! Discord snowflake IDs. The API sends them as strings; we store a u64.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Id(pub u64);

impl Id {
    pub fn timestamp_ms(self) -> u64 {
        (self.0 >> 22) + DISCORD_EPOCH_MS
    }

    /// The smallest ID created at `ms`; used as a message nonce.
    pub fn from_timestamp_ms(ms: u64) -> Id {
        Id(ms.saturating_sub(DISCORD_EPOCH_MS) << 22)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl de::Visitor<'_> for V {
            type Value = Id;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a snowflake as a string or integer")
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Id, E> {
                Ok(Id(v))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Id, E> {
                v.parse().map(Id).map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_string_and_number() {
        let a: Id = serde_json::from_str("\"175928847299117063\"").unwrap();
        let b: Id = serde_json::from_str("175928847299117063").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.0, 175928847299117063);
    }

    #[test]
    fn serializes_as_string() {
        assert_eq!(serde_json::to_string(&Id(42)).unwrap(), "\"42\"");
    }

    #[test]
    fn timestamp_round_trip() {
        // Discord's documented example snowflake.
        assert_eq!(Id(175928847299117063).timestamp_ms(), 1462015105796);
        let id = Id::from_timestamp_ms(1462015105796);
        assert_eq!(id.timestamp_ms(), 1462015105796);
    }

    #[test]
    fn rejects_garbage() {
        assert!(serde_json::from_str::<Id>("\"abc\"").is_err());
    }
}
