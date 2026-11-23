//! The request's JSON tree, read under the protocol's document rules
//! (`reference/library-protocol.md`, Documents).
//!
//! serde_json does the lexing, so the escapes, UTF-8, number grammar, byte
//! order mark and trailing bytes are judged by the same parser the driver
//! uses, and any text that is not JSON is refused there with a line and
//! column. Parsing into `serde_json::Value` would merge duplicate keys and
//! read `-0` as a number, so the document goes through this visitor
//! instead: objects keep every member in document order, so
//! [`crate::request`] can refuse a duplicate with its path, and every number
//! that is not an integer in -2^63 to 2^64-1 (a fraction, an exponent, `-0`,
//! or out of range) is kept as [`Json::NotInteger`], which no field accepts,
//! so its refusal names a path too.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

/// A JSON value as the protocol admits it: integers only, objects with
/// their keys in document order.
#[derive(Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i128),
    /// Any number that is not an integer in -2^63 to 2^64-1.
    NotInteger,
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// What the value is, for a refusal that found it where another was due.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "a boolean",
            Self::Int(_) => "an integer",
            Self::NotInteger => {
                "a number that is not an integer in -2^63 to 2^64-1 (a fraction, an exponent, \
                 -0, or out of range)"
            }
            Self::Str(_) => "a string",
            Self::Arr(_) => "an array",
            Self::Obj(_) => "an object",
        }
    }
}

impl<'de> serde::Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Int(i128::from(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Int(i128::from(v)))
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Json, E> {
        Ok(Json::NotInteger)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<Json>()? {
            items.push(item);
        }
        Ok(Json::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut members = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            members.push((key, value));
        }
        Ok(Json::Obj(members))
    }
}

/// Parse `bytes` as one document: only whitespace may follow it.
pub(crate) fn parse(bytes: &[u8]) -> Result<Json, String> {
    serde_json::from_slice::<Json>(bytes).map_err(|e| format!("the request is not valid JSON: {e}"))
}

/// A key for a refusal: quoted, and cut so an arbitrarily long key cannot
/// push the rest of the reason past the refusal's length limit.
pub(crate) fn shown_key(key: &str) -> String {
    const SHOWN_MAX: usize = 80;
    if key.chars().count() > SHOWN_MAX {
        let cut: String = key.chars().take(SHOWN_MAX).collect();
        format!("{cut:?}...")
    } else {
        format!("{key:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Json, parse};

    #[test]
    fn reads_a_request_shaped_document() {
        let text = r#" {"a": [1, -2, true, null], "b": "xé\n", "a": 0} "#;
        let doc = parse(text.as_bytes()).expect("valid");
        assert_eq!(
            doc,
            Json::Obj(vec![
                (
                    "a".into(),
                    Json::Arr(vec![
                        Json::Int(1),
                        Json::Int(-2),
                        Json::Bool(true),
                        Json::Null
                    ])
                ),
                ("b".into(), Json::Str("x\u{e9}\n".into())),
                ("a".into(), Json::Int(0)),
            ])
        );
    }

    #[test]
    fn numbers_outside_the_integer_domain_survive_as_not_integer() {
        for text in ["-0", "1.0", "1e3", "0e0", "18446744073709551616"] {
            assert_eq!(parse(text.as_bytes()), Ok(Json::NotInteger), "{text}");
        }
        assert_eq!(
            parse(b"18446744073709551615"),
            Ok(Json::Int(i128::from(u64::MAX)))
        );
        assert_eq!(
            parse(b"-9223372036854775808"),
            Ok(Json::Int(i128::from(i64::MIN)))
        );
    }

    #[test]
    fn text_that_is_not_json_is_refused_by_the_parser() {
        for bad in [
            &b"01"[..],
            b"{} x",
            "\u{feff}{}".as_bytes(),
            b"\"a\nb\"",
            b"[1,]",
            b"",
            br#""repeat-c\u+061t""#,
            br#""\u12""#,
            br#""\ud800""#,
            b"\"\xff\"",
        ] {
            let refused = parse(bad).expect_err("must be refused");
            assert!(refused.contains("line"), "{bad:?}: {refused}");
        }
    }

    #[test]
    fn long_strings_parse() {
        let text = format!("\"{}\"", "\u{e9}".repeat(1 << 20));
        assert!(matches!(parse(text.as_bytes()), Ok(Json::Str(s)) if s.len() == 2 << 20));
    }
}
