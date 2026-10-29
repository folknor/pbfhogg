//! A strict JSON reader for the request: the protocol's document rules
//! (`brokkr man library`, Documents) refuse a duplicate key, a number with a
//! fraction or exponent, `-0`, a byte order mark and anything after the
//! document but whitespace. A generic reader would merge duplicate keys and
//! read `-0` as `0` before any check could see them, so the request is read
//! by this one.

/// A JSON value as the protocol admits it: integers only, objects with
/// their keys in document order.
#[derive(Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i128),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Nesting deeper than this is refused rather than recursed into.
const DEPTH_MAX: usize = 64;

/// Parse `text` as one document.
pub(crate) fn parse(text: &str) -> Result<Json, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.at != parser.bytes.len() {
        return Err(format!("trailing bytes at offset {}", parser.at));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, literal: &[u8]) -> bool {
        if self.bytes[self.at..].starts_with(literal) {
            self.at += literal.len();
            true
        } else {
            false
        }
    }

    fn error<T>(&self, what: &str) -> Result<T, String> {
        Err(format!("{what} at offset {}", self.at))
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > DEPTH_MAX {
            return self.error("nesting too deep");
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Json::Str),
            Some(b'-' | b'0'..=b'9') => self.integer(),
            _ if self.eat(b"true") => Ok(Json::Bool(true)),
            _ if self.eat(b"false") => Ok(Json::Bool(false)),
            _ if self.eat(b"null") => Ok(Json::Null),
            _ => self.error("expected a value"),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.at += 1;
        let mut members: Vec<(String, Json)> = Vec::new();
        self.skip_ws();
        if self.eat(b"}") {
            return Ok(Json::Obj(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return self.error("expected a key");
            }
            let key = self.string()?;
            if members.iter().any(|(k, _)| *k == key) {
                return self.error(&format!("duplicate key {key:?}"));
            }
            self.skip_ws();
            if !self.eat(b":") {
                return self.error("expected ':'");
            }
            self.skip_ws();
            let value = self.value(depth + 1)?;
            members.push((key, value));
            self.skip_ws();
            if self.eat(b"}") {
                return Ok(Json::Obj(members));
            }
            if !self.eat(b",") {
                return self.error("expected ',' or '}'");
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.at += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.eat(b"]") {
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            if self.eat(b"]") {
                return Ok(Json::Arr(items));
            }
            if !self.eat(b",") {
                return self.error("expected ',' or ']'");
            }
        }
    }

    /// An integer without fraction, exponent, leading zero or `-0`.
    fn integer(&mut self) -> Result<Json, String> {
        let start = self.at;
        let negative = self.eat(b"-");
        let digits = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        let text = &self.bytes[digits..self.at];
        if text.is_empty() || (text.len() > 1 && text[0] == b'0') || (negative && text == b"0") {
            self.at = start;
            return self.error("not a plain integer");
        }
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return self.error("a number with a fraction or exponent");
        }
        let spelled = std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("");
        spelled
            .parse::<i128>()
            .map(Json::Int)
            .or_else(|_| self.error("an integer out of range"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return self.error("unterminated string");
            };
            match byte {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    out.push(self.escape()?);
                }
                0..=0x1f => return self.error("a control character in a string"),
                _ => {
                    // Copy one whole UTF-8 character; the input is a &str.
                    let rest = std::str::from_utf8(&self.bytes[self.at..]).unwrap_or("");
                    let Some(c) = rest.chars().next() else {
                        return self.error("invalid UTF-8");
                    };
                    out.push(c);
                    self.at += c.len_utf8();
                }
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let Some(byte) = self.peek() else {
            return self.error("unterminated escape");
        };
        self.at += 1;
        Ok(match byte {
            b'"' => '"',
            b'\\' => '\\',
            b'/' => '/',
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => return self.unicode(),
            _ => return self.error("an unknown escape"),
        })
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let Some(digits) = self.bytes.get(self.at..self.at + 4) else {
            return self.error("a short \\u escape");
        };
        let text = std::str::from_utf8(digits).unwrap_or("");
        let value = u32::from_str_radix(text, 16).or_else(|_| self.error("a bad \\u escape"))?;
        self.at += 4;
        Ok(value)
    }

    fn unicode(&mut self) -> Result<char, String> {
        let high = self.hex4()?;
        let code = if (0xD800..0xDC00).contains(&high) {
            if !self.eat(b"\\u") {
                return self.error("an unpaired surrogate");
            }
            let low = self.hex4()?;
            if !(0xDC00..0xE000).contains(&low) {
                return self.error("an unpaired surrogate");
            }
            0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
        } else {
            high
        };
        char::from_u32(code).map_or_else(|| self.error("an invalid code point"), Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::{Json, parse};

    #[test]
    fn reads_a_request_shaped_document() {
        let doc = parse(r#" {"a": [1, -2, true, null], "b": "xé\n"} "#).expect("valid");
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
            ])
        );
    }

    #[test]
    fn refuses_what_the_protocol_refuses() {
        for bad in [
            r#"{"a": 1, "a": 2}"#,
            "-0",
            "01",
            "1.0",
            "1e3",
            "{} x",
            "\u{feff}{}",
            r#""a
b""#,
            "[1,]",
            "",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
