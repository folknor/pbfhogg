use std::collections::hash_map::Entry;
use std::io::Write;
use std::ops::Range;

use rustc_hash::FxHashMap;

/// Export prints the crate's one reading of element metadata.
pub(super) use crate::read::claims::MetaClaims;

use super::ExportOptions;

/// Why formatting a feature stopped.
#[derive(Debug)]
pub(super) enum Stop {
    /// The next append would pass the formatting buffer's cap. Not a
    /// refusal: the block is formatted again where no cap applies.
    Overflow,
    /// The element is refused (a metadata claim export cannot print).
    Refused(crate::Error),
}

impl From<crate::Error> for Stop {
    fn from(error: crate::Error) -> Self {
        Self::Refused(error)
    }
}

/// Fails with [`Stop::Overflow`] when `incoming` more bytes would take
/// `buf` past `cap`.
pub(super) fn reserve(buf: &[u8], incoming: usize, cap: usize) -> Result<(), Stop> {
    if buf.len().saturating_add(incoming) > cap {
        return Err(Stop::Overflow);
    }
    Ok(())
}

fn push_json_string(buf: &mut Vec<u8>, value: &str, cap: usize) -> Result<(), Stop> {
    reserve(buf, json_string_bound(value), cap)?;
    encode_json_string(buf, value)
}

fn json_string_bound(value: &str) -> usize {
    // serde_json escapes a byte as at most six (\u001f), plus the quotes.
    value.len().saturating_mul(6).saturating_add(2)
}

fn encode_json_string(buf: &mut Vec<u8>, value: &str) -> Result<(), Stop> {
    // Use serde_json's data encoding for every destination, including a
    // terminal. Literal Unicode display controls are permitted in JSON;
    // display escaping belongs to the consumer, not the property value.
    serde_json::to_writer(&mut *buf, value)
        .map_err(|error| crate::Error::internal_with("failed to encode a JSON string", error))?;
    Ok(())
}

/// The escaped string-table entries used by one block. The validated table
/// returns stable slices into the block buffer. Hash their address and length
/// so a cache hit never has to read the source string. The address is only a
/// key, and the cache follows the same block if formatting moves to the
/// consumer. Duplicate keys are still detected by string content.
#[derive(Default)]
pub(super) struct EscapedStrings {
    bytes: Vec<u8>,
    entries: FxHashMap<(usize, usize), Range<usize>>,
}

impl EscapedStrings {
    fn push(&mut self, out: &mut Vec<u8>, value: &str, cap: usize) -> Result<(), Stop> {
        // Keep the same cap decision on cache hits, including when the
        // worker hands the block to the consumer for a retry.
        reserve(out, json_string_bound(value), cap)?;
        let key = (value.as_ptr().addr(), value.len());
        let range = match self.entries.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let start = self.bytes.len();
                encode_json_string(&mut self.bytes, value)?;
                entry.insert(start..self.bytes.len())
            }
        };
        let bytes = self.bytes.get(range.clone()).ok_or_else(|| {
            crate::Error::internal("export escaped string cache range is out of bounds")
        })?;
        out.extend_from_slice(bytes);
        Ok(())
    }
}

fn push_name(buf: &mut Vec<u8>, first: &mut bool, name: &str, cap: usize) -> Result<(), Stop> {
    if !*first {
        buf.push(b',');
    }
    *first = false;
    push_json_string(buf, name, cap)?;
    buf.push(b':');
    Ok(())
}

/// Whether a tag of this key would collide with an emitted reserved name.
/// The metadata names are reserved under `--metadata` whether or not the
/// element carries the claim.
fn is_reserved(key: &str, metadata: bool) -> bool {
    matches!(key, "@id" | "@type")
        || (metadata
            && matches!(
                key,
                "@version" | "@timestamp" | "@changeset" | "@uid" | "@user" | "@visible"
            ))
}

/// Keys already emitted for one feature, so the first occurrence of a
/// duplicate source key wins. A linear scan over the few keys an element
/// carries, switching to a hash set past a handful so a pathological
/// element is not quadratic. Reused across one block's features.
#[derive(Default)]
pub(super) struct SeenKeys<'a> {
    few: Vec<&'a str>,
    many: std::collections::HashSet<&'a str>,
}

/// Keys past which [`SeenKeys`] hashes.
const SEEN_LINEAR_MAX: usize = 16;

impl<'a> SeenKeys<'a> {
    fn clear(&mut self) {
        self.few.clear();
        self.many.clear();
    }

    /// Records `key`; `false` when it was already recorded.
    fn insert(&mut self, key: &'a str) -> bool {
        if self.few.len() < SEEN_LINEAR_MAX {
            if self.few.contains(&key) {
                return false;
            }
            self.few.push(key);
            return true;
        }
        if self.few.contains(&key) {
            return false;
        }
        self.many.insert(key)
    }
}

/// Writes one feature's properties object to `buf`, appending only while
/// `buf` stays within `cap`. `meta` is consulted only under `--metadata`.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_properties<'a, T>(
    buf: &mut Vec<u8>,
    seen: &mut SeenKeys<'a>,
    escaped: &mut EscapedStrings,
    id: i64,
    object_type: &str,
    tags: T,
    meta: Option<&MetaClaims<'a>>,
    opts: &ExportOptions,
    cap: usize,
) -> Result<(), Stop>
where
    T: Iterator<Item = (&'a str, &'a str)>,
{
    // The fixed names and numbers below are short; one reservation covers
    // them, and every string carries its own.
    reserve(buf, 256, cap)?;
    buf.push(b'{');
    let mut first = true;
    push_name(buf, &mut first, "@id", cap)?;
    write!(buf, "{id}").ok();
    push_name(buf, &mut first, "@type", cap)?;
    push_json_string(buf, object_type, cap)?;

    if opts.metadata
        && let Some(meta) = meta
    {
        if let Some(value) = meta.version {
            push_name(buf, &mut first, "@version", cap)?;
            write!(buf, "{value}").ok();
        }
        if let Some(millis) = &meta.milli_timestamp {
            // Scaled exactly: an overflow is refused, not printed as
            // the wrapped date.
            let millis = *millis
                .as_ref()
                .map_err(|err| err.clone().context(format!("{object_type} {id}")))?;
            // RFC 3339 is promised, so a year outside 0000 to 9999 is
            // refused rather than spelled with five digits or a sign. A
            // sub-second instant keeps its milliseconds, so a non-zero
            // claim never prints as the epoch.
            let text = crate::commands::format_rfc3339_utc_millis(millis).map_err(|err| {
                crate::Error::invalid_data_from(err).context(format!("{object_type} {id}"))
            })?;
            push_name(buf, &mut first, "@timestamp", cap)?;
            push_json_string(buf, &text, cap)?;
        }
        if let Some(value) = meta.changeset {
            push_name(buf, &mut first, "@changeset", cap)?;
            write!(buf, "{value}").ok();
        }
        if let Some(value) = meta.uid {
            push_name(buf, &mut first, "@uid", cap)?;
            write!(buf, "{value}").ok();
        }
        if let Some(user) = &meta.user {
            push_name(buf, &mut first, "@user", cap)?;
            escaped.push(buf, user.as_ref().map_err(crate::Error::clone)?, cap)?;
        }
        if let Some(value) = meta.visible {
            push_name(buf, &mut first, "@visible", cap)?;
            buf.extend_from_slice(if value { b"true" } else { b"false" });
        }
    }

    seen.clear();
    for (key, value) in tags {
        if is_reserved(key, opts.metadata)
            || !seen.insert(key)
            || opts
                .properties
                .as_ref()
                .is_some_and(|keys| !keys.contains(key))
        {
            continue;
        }
        buf.push(b',');
        escaped.push(buf, key, cap)?;
        buf.push(b':');
        escaped.push(buf, value, cap)?;
    }
    buf.push(b'}');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::export::{ExportFormat, ExportTypes};

    #[test]
    fn cached_strings_are_lazy_and_share_their_escaped_bytes() {
        let mut builder = crate::block_builder::BlockBuilder::new();
        let value = "a\n\"\\";
        builder.add_node(1, 0, 0, [(value, value), ("unused", "tag")], None);
        let bytes = builder.take().expect("take").expect("block").to_vec();
        let block = crate::PrimitiveBlock::from_vec(bytes).expect("decode");
        let Some(crate::Element::DenseNode(node)) = block.elements().next() else {
            panic!("expected a dense node");
        };
        let (key, value) = node.tags().next().expect("tag");

        let mut escaped = EscapedStrings::default();
        let mut out = Vec::new();
        assert!(matches!(
            escaped.push(&mut out, key, 1),
            Err(Stop::Overflow)
        ));
        assert!(escaped.entries.is_empty());
        escaped.push(&mut out, key, usize::MAX).expect("key");
        escaped.push(&mut out, value, usize::MAX).expect("value");
        let want = serde_json::to_vec(value).expect("JSON");
        assert_eq!(escaped.entries.len(), 1);
        assert_eq!(escaped.bytes, want);
        assert_eq!(out, [want.as_slice(), want.as_slice()].concat());
    }

    #[test]
    fn escapes_whitelists_and_suppresses_collisions() {
        let options = ExportOptions::new(
            ExportFormat::GeoJsonSeq,
            ExportTypes::All,
            &[],
            Some(vec!["name".to_owned(), "@id".to_owned()]),
            None,
            false,
        )
        .expect("options");
        let tags = [
            ("@id", "wrong"),
            ("name", "a \"quote\" and \\ slash"),
            ("name", "duplicate"),
            ("drop", "me"),
        ];
        let mut buf = Vec::new();
        let mut seen = SeenKeys::default();
        write_properties(
            &mut buf,
            &mut seen,
            &mut EscapedStrings::default(),
            7,
            "node",
            tags.into_iter(),
            None,
            &options,
            usize::MAX,
        )
        .expect("properties");
        assert_eq!(
            String::from_utf8(buf).expect("JSON is UTF-8"),
            "{\"@id\":7,\"@type\":\"node\",\"name\":\"a \\\"quote\\\" and \\\\ slash\"}"
        );
    }

    /// The first occurrence of a key wins past the linear scan's reach too.
    #[test]
    fn many_keys_keep_first_occurrence() {
        let options = ExportOptions::new(
            ExportFormat::GeoJsonSeq,
            ExportTypes::All,
            &[],
            None,
            None,
            false,
        )
        .expect("options");
        let keys: Vec<String> = (0..40).map(|i| format!("k{i}")).collect();
        let tags: Vec<(&str, &str)> = keys
            .iter()
            .map(|k| (k.as_str(), "first"))
            .chain(keys.iter().map(|k| (k.as_str(), "second")))
            .collect();
        let mut buf = Vec::new();
        let mut seen = SeenKeys::default();
        write_properties(
            &mut buf,
            &mut seen,
            &mut EscapedStrings::default(),
            1,
            "node",
            tags.into_iter(),
            None,
            &options,
            usize::MAX,
        )
        .expect("properties");
        let text = String::from_utf8(buf).expect("JSON is UTF-8");
        assert_eq!(text.matches("\"first\"").count(), 40);
        assert!(!text.contains("second"));
    }

    /// A string that would pass the cap stops before it is appended.
    #[test]
    fn cap_stops_before_the_append() {
        let options = ExportOptions::new(
            ExportFormat::GeoJsonSeq,
            ExportTypes::All,
            &[],
            None,
            None,
            false,
        )
        .expect("options");
        let long = "x".repeat(10_000);
        let mut buf = Vec::new();
        let mut seen = SeenKeys::default();
        let result = write_properties(
            &mut buf,
            &mut seen,
            &mut EscapedStrings::default(),
            1,
            "node",
            [("name", long.as_str())].into_iter(),
            None,
            &options,
            1_000,
        );
        assert!(matches!(result, Err(Stop::Overflow)));
        assert!(buf.len() <= 1_000);
    }
}
