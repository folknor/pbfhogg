//! Fixture PBFs for the generated tool protocols: a valid header-only PBF,
//! so the verifier's own header and framing readers read it, whose
//! `HeaderBlock` lists its optional features and ends with a marker string
//! the fake tools key their behaviour on. The fake osmium decodes a fixture
//! to one OPL record carrying its marker, so two fixtures compare equal
//! exactly when their markers are equal, and it decodes by the requested
//! OPL options ([`project`]), so a fixture whose version alone differs
//! ([`alter`]) differs exactly under a policy that reads metadata.
use std::fmt::Write as _;
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

/// The whole marker of a fixture written by [`write()`], field suffixes
/// included.
fn full_marker(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    let at = bytes
        .windows(MARKER.len())
        .rposition(|w| w == MARKER)
        .unwrap_or_else(|| panic!("{} is not a fixture PBF", path.display()));
    let text = &bytes[at + MARKER.len()..];
    // The marker runs to the end of a header-only fixture, or to the frame
    // length of the data blob [`write_indexed`] appends, whose leading byte
    // is zero; a marker never holds one.
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    String::from_utf8(text[..end].to_vec()).unwrap()
}

/// [`write()`] followed by one raw data blob with indexdata, so the
/// verifier reads the fixture's first data blob as indexed. The blob holds
/// dense node 1; the fake osmium decodes by the marker alone, so the blob
/// changes no decoding, only the indexed claim a reader of the frames sees.
pub fn write_indexed(path: &Path, optional: &[&str], marker: &str) {
    write(path, optional, marker);
    let mut dense = Vec::new();
    field(1, &[2], &mut dense);
    let mut group = Vec::new();
    field(2, &dense, &mut group);
    let mut primitive = Vec::new();
    field(1, &[], &mut primitive);
    field(2, &group, &mut primitive);
    let mut blob = Vec::new();
    field(1, &primitive, &mut blob);
    // Indexdata: kind node, IDs 1..1, one element.
    let mut index = vec![1, 0];
    index.extend_from_slice(&1_i64.to_le_bytes());
    index.extend_from_slice(&1_i64.to_le_bytes());
    index.extend_from_slice(&1_u64.to_le_bytes());
    let mut header = Vec::new();
    field(1, b"OSMData", &mut header);
    field(2, &index, &mut header);
    varint(3 << 3, &mut header);
    varint(blob.len() as u64, &mut header);
    let mut out = std::fs::read(path).unwrap();
    out.extend_from_slice(&u32::try_from(header.len()).unwrap().to_be_bytes());
    out.extend(header);
    out.extend(blob);
    std::fs::write(path, out).unwrap();
}

/// The content marker of a fixture written by [`write()`]: the marker
/// without its field suffixes (see [`alter`]).
pub fn marker(path: &Path) -> String {
    let full = full_marker(path);
    full.split(" @").next().unwrap().to_owned()
}

/// A fixture's field suffixes as written (` @v2 @w1`), empty when none: what
/// a fake tool that rewrites content but keeps fields carries over.
pub fn suffixes(path: &Path) -> String {
    let full = full_marker(path);
    full.find(" @")
        .map_or_else(String::new, |at| full[at..].to_owned())
}

/// The fields a fixture decodes with, from its marker's suffixes, the last
/// of each kind winning: the version (`@v<N>`, default 1) and, when an
/// `@w<X>` suffix is present, a way whose one node carries the inline
/// longitude `X`. The way's presence is content and never follows the
/// `LocationsOnWays` declaration, so dropping the declaration does not
/// remove an identity.
fn fields(path: &Path) -> (String, Option<String>) {
    let full = full_marker(path);
    let mut version = "1".to_owned();
    let mut way = None;
    for suffix in full.split(" @").skip(1) {
        if let Some(v) = suffix.strip_prefix('v') {
            v.clone_into(&mut version);
        } else if let Some(x) = suffix.strip_prefix('w') {
            way = Some(x.to_owned());
        } else {
            panic!("unknown fixture field suffix {suffix:?}");
        }
    }
    (version, way)
}

/// Copy fixture `src` to `dst` (features, content and fields) with
/// `suffix` (`@v<version>` or `@w<longitude>`) appended, so `dst` decodes
/// with its metadata or its way's inline coordinate altered and everything
/// else equal.
pub fn alter(src: &Path, dst: &Path, suffix: &str) {
    let features = features(src);
    let refs: Vec<&str> = features.iter().map(String::as_str).collect();
    write(dst, &refs, &format!("{} {suffix}", full_marker(src)));
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
/// input's content marker (its non-alphanumeric bytes as `_`) and its
/// version, then the way its fields name, [`project`]ed by the requested
/// format.
pub fn opl_cat(args: &[&str]) {
    let input = Path::new(args[1]);
    let output = args[args.iter().position(|a| *a == "-o").unwrap() + 1];
    let content: String = marker(input)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let (version, way) = fields(input);
    let mut opl = format!("n1 v{version} dV c1 t i1 u Tm={content} x1 y1\n");
    if let Some(x) = way {
        writeln!(opl, "w1 v{version} dV c1 t i1 u T Nn1x{x}y1").unwrap();
    }
    std::fs::write(output, project(args, &opl)).unwrap();
}

/// `opl`, written with every field, as osmium writes it under the `-f`
/// format in `args`: `add_metadata=false` drops the version, visibility,
/// changeset, timestamp, uid and user fields; without
/// `locations_on_ways=true` way-node items keep their refs and lose their
/// inline coordinates. Node coordinates are content and always stay.
pub fn project(args: &[&str], opl: &str) -> String {
    let format = args[args.iter().position(|a| *a == "-f").unwrap() + 1];
    let options: Vec<&str> = format.split(',').collect();
    assert_eq!(options[0], "opl", "not an OPL decode: {format}");
    let metadata = !options.contains(&"add_metadata=false");
    let locations = options.contains(&"locations_on_ways=true");
    for option in &options[1..] {
        assert!(
            ["add_metadata=false", "locations_on_ways=true"].contains(option),
            "unexpected OPL option {option}"
        );
    }
    let mut out = String::new();
    for line in opl.lines() {
        let mut fields = line.split(' ');
        let mut kept = vec![fields.next().unwrap().to_owned()];
        let way = kept[0].starts_with('w');
        for field in fields {
            let letter = field.chars().next().unwrap_or(' ');
            if !metadata && "vdctiu".contains(letter) {
                continue;
            }
            if way && letter == 'N' && !locations {
                let items: Vec<&str> = field[1..]
                    .split(',')
                    .filter(|i| !i.is_empty())
                    .map(|i| i.split('x').next().unwrap())
                    .collect();
                kept.push(format!("N{}", items.join(",")));
                continue;
            }
            kept.push(field.to_owned());
        }
        out.push_str(&kept.join(" "));
        out.push('\n');
    }
    out
}
