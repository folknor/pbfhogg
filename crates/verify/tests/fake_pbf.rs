//! Fixture PBFs for the generated tool protocols: a valid header-only PBF,
//! so the verifier's own header and framing readers read it, whose
//! `HeaderBlock` lists its optional features and ends with a marker string
//! the fake tools key their behaviour on. The fake osmium decodes a fixture
//! to one OPL record carrying its marker, so two fixtures compare equal
//! exactly when their markers are equal.
use std::path::Path;

const MARKER: &[u8] = b"marker:";

fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push(u8::try_from(v & 0x7f).unwrap() | 0x80);
        v >>= 7;
    }
    out.push(u8::try_from(v).unwrap());
}

fn field(number: u64, bytes: &[u8], out: &mut Vec<u8>) {
    varint(number << 3 | 2, out);
    varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

/// Write a header-only PBF at `path` claiming `optional` features and
/// carrying `marker`.
pub fn write(path: &Path, optional: &[&str], marker: &str) {
    write_with(path, &[], optional, marker);
}

/// [`write()`] with `required` features too.
pub fn write_with(path: &Path, required: &[&str], optional: &[&str], marker: &str) {
    let mut block = Vec::new();
    for feature in required {
        field(4, feature.as_bytes(), &mut block);
    }
    for feature in optional {
        field(5, feature.as_bytes(), &mut block);
    }
    let mut text = MARKER.to_vec();
    text.extend_from_slice(marker.as_bytes());
    // Field 16, writingprogram, last.
    field(16, &text, &mut block);
    let mut blob = Vec::new();
    field(1, &block, &mut blob);
    let mut header = Vec::new();
    field(1, b"OSMHeader", &mut header);
    varint(3 << 3, &mut header);
    varint(blob.len() as u64, &mut header);
    let mut out = u32::try_from(header.len()).unwrap().to_be_bytes().to_vec();
    out.extend(header);
    out.extend(blob);
    std::fs::write(path, out).unwrap();
}

/// The marker of a fixture written by [`write()`].
pub fn marker(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    let at = bytes
        .windows(MARKER.len())
        .rposition(|w| w == MARKER)
        .unwrap_or_else(|| panic!("{} is not a fixture PBF", path.display()));
    String::from_utf8(bytes[at + MARKER.len()..].to_vec()).unwrap()
}

/// The optional features of a fixture written by [`write()`].
pub fn features(path: &Path) -> Vec<String> {
    crate::pbf_frames::header(path).unwrap().optional
}

/// Copy `src`'s features to a new fixture at `dst` with `marker`.
pub fn rewrite(src: &Path, dst: &Path, marker: &str) {
    let features = features(src);
    let refs: Vec<&str> = features.iter().map(String::as_str).collect();
    write(dst, &refs, marker);
}

/// osmium fileinfo's report of an ordered file with these counts.
pub fn fileinfo(nodes: u64, ways: u64, relations: u64) -> String {
    format!(
        r#"{{"data":{{"objects_ordered":true,"multiple_versions":false,"count":{{"nodes":{nodes},"ways":{ways},"relations":{relations}}},"minid":{{"nodes":1,"ways":1,"relations":1}},"maxid":{{"nodes":{nodes},"ways":{ways},"relations":{relations}}}}}}}"#
    )
}

/// Whether `args` is the comparator's `osmium cat ... -f opl...`.
pub fn is_opl_cat(args: &[&str]) -> bool {
    args.first() == Some(&"cat")
        && args
            .windows(2)
            .any(|w| w[0] == "-f" && w[1].starts_with("opl"))
}

/// Answer an [`is_opl_cat`] invocation: one node record carrying the
/// input's marker (its non-alphanumeric bytes as `_`).
pub fn opl_cat(args: &[&str]) {
    let input = Path::new(args[1]);
    let output = args[args.iter().position(|a| *a == "-o").unwrap() + 1];
    let marker: String = marker(input)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::fs::write(output, format!("n1 v1 Tm={marker} x1 y1\n")).unwrap();
}
