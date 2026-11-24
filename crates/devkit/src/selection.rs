//! Optional dataset selections. Absence is distinct from invalid configuration.
use std::collections::BTreeMap;

use crate::datasets::{child_table, optional_string};
use crate::registry::osc_entries;
use crate::{Config, Error, ResolvedInput};

/// Four finite coordinates, preserving the spelling used on command lines.
/// As in brokkr, coordinate ranges and min/max ordering are not constrained.
#[derive(Debug, Clone, PartialEq)]
pub struct Bbox {
    coordinates: [f64; 4],
    source: String,
}

impl Bbox {
    /// Parse longitude minimum, latitude minimum, longitude maximum, latitude maximum.
    pub fn parse(value: &str) -> Result<Self, Error> {
        let parts: Vec<_> = value.split(',').collect();
        if parts.len() != 4 {
            return Err(Error::message(
                "bbox requires four comma-separated coordinates",
            ));
        }
        let mut coordinates = [0.0; 4];
        for (index, part) in parts.iter().enumerate() {
            let number: f64 = part
                .trim()
                .parse()
                .map_err(|_| Error::message(format!("invalid bbox component {index}: '{part}'")))?;
            if !number.is_finite() {
                return Err(Error::message(format!(
                    "bbox component {index} must be finite"
                )));
            }
            coordinates[index] = number;
        }
        Ok(Self {
            coordinates,
            source: value.to_owned(),
        })
    }

    /// Original spelling.
    pub fn as_str(&self) -> &str {
        &self.source
    }
    /// Coordinates in longitude/latitude order.
    pub fn coordinates(&self) -> [f64; 4] {
        self.coordinates
    }
    /// Single-token CLI option, also safe for negative west longitudes.
    pub fn to_pbfhogg_arg(&self) -> String {
        format!("-b={}", self.source)
    }

    /// The same four decimals spelled so libosmium's coordinate parser reads
    /// each as the column pbfhogg reads from the original text, or `None`
    /// when an edge has more than ten integer digits (no coordinate does).
    ///
    /// pbfhogg rounds an edge's exact decimal value half away from zero to a
    /// decimicrodegree, deciding on the eighth decimal. libosmium reads at
    /// most eight fraction digits and rounds the same way, but applies an
    /// exponent to the already truncated mantissa, and refuses a `+`,
    /// whitespace and long leading-zero runs. So each edge is restated as a
    /// plain decimal: the exponent applied exactly to the digit string, the
    /// fraction truncated (never rounded) after its eighth digit. That is the
    /// same decimal for every decision either parser makes, so no column is
    /// computed here. A plain edge with at most eight decimals keeps its
    /// spelling, apart from leading integer zeros.
    pub fn to_osmium_text(&self) -> Option<String> {
        let edges: Option<Vec<String>> = self.source.split(',').map(osmium_coordinate).collect();
        Some(edges?.join(","))
    }
}

/// One edge as a plain decimal libosmium reads exactly; see
/// [`Bbox::to_osmium_text`]. The token already parsed as a finite f64.
fn osmium_coordinate(token: &str) -> Option<String> {
    let s = token.trim();
    let (negative, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    // Saturated past the token's own length, which no mantissa can cancel.
    let cap = i64::try_from(s.len()).ok()?.saturating_add(20);
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(at) => {
            let e = &s[at + 1..];
            let (e_negative, e) = match e.as_bytes().first() {
                Some(b'-') => (true, &e[1..]),
                Some(b'+') => (false, &e[1..]),
                _ => (false, e),
            };
            if e.is_empty() || !e.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let value = e
                .bytes()
                .fold(0_i64, |acc, b| (acc * 10 + i64::from(b - b'0')).min(cap));
            (&s[..at], if e_negative { -value } else { value })
        }
        None => (s, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits: Vec<u8> = int_part
        .bytes()
        .chain(frac_part.bytes())
        .skip_while(|&b| b == b'0')
        .collect();
    if digits.is_empty() {
        // Zero, however large its exponent.
        return Some("0".to_owned());
    }
    let len = i64::try_from(digits.len()).ok()?;
    let frac_len = i64::try_from(frac_part.len()).ok()?;
    // Digits before the point once the exponent is applied.
    let point = len - frac_len + exponent;
    if point > 10 {
        return None;
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if point <= -8 {
        out.push('0');
        return Some(out);
    }
    let (int_digits, frac_digits): (String, String) = if point >= 0 {
        let point = usize::try_from(point).ok()?;
        let mut int: String = digits.iter().take(point).map(|&b| char::from(b)).collect();
        while int.len() < point {
            int.push('0');
        }
        let frac = digits
            .iter()
            .skip(point)
            .take(8)
            .map(|&b| char::from(b))
            .collect();
        (int, frac)
    } else {
        let zeros = usize::try_from(-point).ok()?;
        let frac = std::iter::repeat_n(b'0', zeros)
            .chain(digits.iter().copied())
            .take(8)
            .map(char::from)
            .collect();
        (String::new(), frac)
    };
    if int_digits.is_empty() {
        out.push('0');
    } else {
        out.push_str(&int_digits);
    }
    if !frac_digits.is_empty() {
        out.push('.');
        out.push_str(&frac_digits);
    }
    Some(out)
}

impl std::str::FromStr for Bbox {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

fn optional_table<'a>(table: &'a toml::Table, key: &str) -> Result<Option<&'a toml::Table>, Error> {
    table
        .get(key)
        .map(|v| {
            v.as_table()
                .ok_or_else(|| Error::message(format!("'{key}' must be a table")))
        })
        .transpose()
}

impl Config {
    fn dataset_table(&self, host: &str, dataset: &str) -> Result<&toml::Table, Error> {
        child_table(child_table(self.selected_host(host)?, "datasets")?, dataset)
    }

    fn selection_table(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
    ) -> Result<&toml::Table, Error> {
        let dataset = self.dataset_table(host, dataset)?;
        match snapshot.filter(|s| *s != "base") {
            None => Ok(dataset),
            Some(key) => {
                if key.is_empty()
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(Error::message(format!("invalid snapshot key '{key}'")));
                }
                child_table(child_table(dataset, "snapshot")?, key)
            }
        }
    }

    /// Resolve an optional PBF variant. A configured but invalid entry is an error.
    pub fn select_pbf(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
        variant: &str,
    ) -> Result<Option<ResolvedInput>, Error> {
        let selected = self.selection_table(host, dataset, snapshot)?;
        match optional_table(selected, "pbf")? {
            Some(table) if table.contains_key(variant) => self
                .resolve_input(host, dataset, snapshot, "input", variant)
                .map(Some),
            _ => Ok(None),
        }
    }

    /// Select a single OSC by sequence, or the sole entry when omitted.
    /// An empty table is absent only without an explicit sequence request.
    pub fn select_osc(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
        sequence: Option<u64>,
    ) -> Result<Option<ResolvedInput>, Error> {
        let table = self.osc_table(host, dataset, snapshot)?;
        let (sequence, entry) = match sequence {
            Some(seq) => {
                let entry = table
                    .as_ref()
                    .and_then(|t| t.get(&seq))
                    .ok_or_else(|| Error::message(format!("missing osc.{seq}")))?;
                (seq, *entry)
            }
            None => match table {
                None => return Ok(None),
                Some(t) if t.is_empty() => return Ok(None),
                Some(t) if t.len() == 1 => t
                    .into_iter()
                    .next()
                    .ok_or_else(|| Error::message("OSC selection disappeared"))?,
                Some(t) => {
                    return Err(Error::message(format!(
                        "multiple OSC entries ({}); select --osc-seq",
                        t.keys().map(u64::to_string).collect::<Vec<_>>().join(", ")
                    )));
                }
            },
        };
        self.resolve_entry(host, "osc", &sequence.to_string(), entry)
            .map(Some)
    }

    /// Whether the selected OSC table has any entries. Never falls back.
    pub fn has_osc(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
    ) -> Result<bool, Error> {
        Ok(self
            .osc_table(host, dataset, snapshot)?
            .is_some_and(|t| !t.is_empty()))
    }

    fn osc_table(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
    ) -> Result<Option<BTreeMap<u64, &toml::Table>>, Error> {
        osc_entries(self.selection_table(host, dataset, snapshot)?)
    }

    /// Resolve every integer in an inclusive OSC range, in ascending order.
    /// Missing sequences, even in an empty table, are errors.
    pub fn select_osc_range(
        &self,
        host: &str,
        dataset: &str,
        snapshot: Option<&str>,
        lo: u64,
        hi: u64,
    ) -> Result<Vec<ResolvedInput>, Error> {
        if lo > hi {
            return Err(Error::message("OSC range must be ascending"));
        }
        let table = self.osc_table(host, dataset, snapshot)?.unwrap_or_default();
        (lo..=hi)
            .map(|seq| {
                let entry = table
                    .get(&seq)
                    .ok_or_else(|| Error::message(format!("missing osc.{seq}")))?;
                self.resolve_entry(host, "osc", &seq.to_string(), entry)
            })
            .collect()
    }

    /// Dataset bbox. Missing is optional; malformed is an error.
    pub fn select_bbox(&self, host: &str, dataset: &str) -> Result<Option<Bbox>, Error> {
        optional_string(self.dataset_table(host, dataset)?, "bbox")?
            .map(Bbox::parse)
            .transpose()
    }

    /// A named polygon, with the same file and pin validation as PBF inputs.
    pub fn select_polygon(
        &self,
        host: &str,
        dataset: &str,
        name: &str,
    ) -> Result<Option<ResolvedInput>, Error> {
        let Some(table) = optional_table(self.dataset_table(host, dataset)?, "polygon")? else {
            return Ok(None);
        };
        if !table.contains_key(name) {
            return Ok(None);
        }
        self.resolve_entry(host, "polygon", name, child_table(table, name)?)
            .map(Some)
    }

    /// All configured polygons in name order; an empty table is absent.
    pub fn select_polygons(
        &self,
        host: &str,
        dataset: &str,
    ) -> Result<Option<Vec<ResolvedInput>>, Error> {
        let Some(table) = optional_table(self.dataset_table(host, dataset)?, "polygon")? else {
            return Ok(None);
        };
        if table.is_empty() {
            return Ok(None);
        }
        let mut names: Vec<_> = table.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| self.resolve_entry(host, "polygon", name, child_table(table, name)?))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }
}
