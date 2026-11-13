//! Optional dataset selections. Absence is distinct from invalid configuration.
use crate::datasets::{child_table, optional_string};
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
        sequence: Option<&str>,
    ) -> Result<Option<ResolvedInput>, Error> {
        let table = self.osc_table(host, dataset, snapshot)?;
        let sequence = match sequence {
            Some(seq) => seq,
            None => match table {
                None => return Ok(None),
                Some(t) if t.is_empty() => return Ok(None),
                Some(t) if t.len() == 1 => t
                    .keys()
                    .next()
                    .ok_or_else(|| Error::message("OSC selection disappeared"))?,
                Some(t) => {
                    return Err(Error::message(format!(
                        "multiple OSC entries ({}); select --osc-seq",
                        t.keys().cloned().collect::<Vec<_>>().join(", ")
                    )));
                }
            },
        };
        let entry = child_table(
            table.ok_or_else(|| Error::message(format!("missing osc.{sequence}")))?,
            sequence,
        )?;
        self.resolve_entry(host, "osc", sequence, entry).map(Some)
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
    ) -> Result<Option<&toml::Table>, Error> {
        optional_table(self.selection_table(host, dataset, snapshot)?, "osc")
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
        (lo..=hi)
            .map(|seq| {
                self.select_osc(host, dataset, snapshot, Some(&seq.to_string()))?
                    .ok_or_else(|| Error::message(format!("missing osc.{seq}")))
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
