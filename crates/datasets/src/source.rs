//! Where a dataset comes from, and which dataset key a region names.
//!
//! A dataset's source is its identity: the PBF URL and the replication
//! stream its OSC sequence numbers live in both follow from it. The registry
//! records it in `origin`, exactly (`download.geofabrik.de/PATH` or
//! `planet.openstreetmap.org`), so two regions sharing a basename (Geofabrik
//! `asia/georgia` and `north-america/us/georgia`) can never be mixed under
//! one key. The legacy value `Geofabrik` names no path; it is read as the
//! alias path of an alias key and refused for any other key.

use pbfhogg_devkit::DatasetView;

const PLANET: &str = "planet.openstreetmap.org";
const GEOFABRIK: &str = "download.geofabrik.de";
const LEGACY_GEOFABRIK: &str = "Geofabrik";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// Geofabrik regional extract, e.g. `europe/denmark`.
    Geofabrik(String),
    /// The full planet from planet.openstreetmap.org.
    Planet,
}

impl Source {
    pub(crate) fn stream_url(&self) -> String {
        match self {
            Self::Geofabrik(path) => format!("https://{GEOFABRIK}/{path}-updates"),
            Self::Planet => format!("https://{PLANET}/replication/day"),
        }
    }

    pub(crate) fn osc_url(&self, seq: u64) -> String {
        format!("{}/{}.osc.gz", self.stream_url(), seq_path(seq))
    }

    pub(crate) fn pbf_url(&self) -> String {
        match self {
            Self::Geofabrik(path) => format!("https://{GEOFABRIK}/{path}-latest.osm.pbf"),
            Self::Planet => format!("https://{PLANET}/pbf/planet-latest.osm.pbf"),
        }
    }

    /// The exact `origin` value the registry records for this source.
    pub(crate) fn origin(&self) -> String {
        match self {
            Self::Geofabrik(path) => format!("{GEOFABRIK}/{path}"),
            Self::Planet => PLANET.to_owned(),
        }
    }

    /// Read a registered `origin` for dataset `key`.
    fn from_origin(origin: &str, key: &str) -> Result<Self, String> {
        if origin == PLANET {
            return Ok(Self::Planet);
        }
        if let Some(path) = origin
            .strip_prefix(GEOFABRIK)
            .and_then(|p| p.strip_prefix('/'))
            && !path.is_empty()
        {
            return Ok(Self::Geofabrik(path.trim_end_matches('/').to_owned()));
        }
        if origin == LEGACY_GEOFABRIK {
            return alias(key)
                .map(|path| Self::Geofabrik(path.to_owned()))
                .ok_or_else(|| {
                    format!(
                        "dataset '{key}' has origin \"{LEGACY_GEOFABRIK}\", which names no region; \
                     set origin = \"{GEOFABRIK}/<path>\" in its registry entry"
                    )
                });
        }
        Err(format!(
            "dataset '{key}' has origin \"{origin}\", which is not a source this tool knows \
             (\"{PLANET}\" or \"{GEOFABRIK}/<path>\")"
        ))
    }
}

/// Short names for commonly used Geofabrik regions.
const ALIASES: &[(&str, &str)] = &[
    ("malta", "europe/malta"),
    (
        "greater-london",
        "europe/united-kingdom/england/greater-london",
    ),
    ("switzerland", "europe/switzerland"),
    ("norway", "europe/norway"),
    ("japan", "asia/japan"),
    ("denmark", "europe/denmark"),
    ("germany", "europe/germany"),
    ("north-america", "north-america"),
    ("europe", "europe"),
];

fn alias(name: &str) -> Option<&'static str> {
    ALIASES.iter().find(|(n, _)| *n == name).map(|(_, p)| *p)
}

/// A region argument resolved against the registry.
#[derive(Debug)]
pub(crate) struct Resolved {
    pub(crate) key: String,
    pub(crate) source: Source,
    pub(crate) dataset: Option<DatasetView>,
}

/// Resolve `region` (`planet`, an alias, a Geofabrik path, or the key of a
/// registered dataset) once into its dataset key, then its source. The key
/// never depends on the registry; a registered dataset's recorded source must
/// equal the requested one.
pub(crate) fn resolve(
    region: &str,
    lookup: impl FnOnce(&str) -> Result<Option<DatasetView>, String>,
) -> Result<Resolved, String> {
    let (key, requested) = if region == "planet" {
        ("planet".to_owned(), Some(Source::Planet))
    } else if let Some(path) = alias(region) {
        (region.to_owned(), Some(Source::Geofabrik(path.to_owned())))
    } else if region.contains('/') {
        let path = region.trim_matches('/');
        let key = path.rsplit('/').next().unwrap_or(path);
        (key.to_owned(), Some(Source::Geofabrik(path.to_owned())))
    } else {
        (region.to_owned(), None)
    };
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "cannot use '{key}' (from region '{region}') as a dataset key: \
             letters, digits, '_' and '-' only"
        ));
    }
    let dataset = lookup(&key)?;
    let recorded = match &dataset {
        None => None,
        Some(view) => Some(Source::from_origin(
            view.origin.as_deref().ok_or_else(|| {
                format!(
                    "dataset '{key}' has no origin; set origin = \"{PLANET}\" or \
                     \"{GEOFABRIK}/<path>\" in its registry entry"
                )
            })?,
            &key,
        )?),
    };
    let source = match (requested, recorded) {
        (Some(want), Some(have)) if want != have => {
            return Err(format!(
                "dataset '{key}' is registered from {}, not {}; a key holds one source",
                have.origin(),
                want.origin()
            ));
        }
        (Some(source), _) | (None, Some(source)) => source,
        (None, None) => {
            let names: Vec<&str> = ALIASES.iter().map(|(n, _)| *n).collect();
            return Err(format!(
                "unknown region '{region}': use 'planet', a Geofabrik path \
                 (e.g. europe/france), a registered dataset, or one of: {}",
                names.join(", ")
            ));
        }
    };
    Ok(Resolved {
        key,
        source,
        dataset,
    })
}

/// Replication paths use three groups, each padded to at least three digits.
fn seq_path(seq: u64) -> String {
    format!(
        "{:03}/{:03}/{:03}",
        seq / 1_000_000,
        (seq / 1_000) % 1_000,
        seq % 1_000
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn view(origin: Option<&str>) -> DatasetView {
        DatasetView {
            origin: origin.map(str::to_owned),
            ..DatasetView::default()
        }
    }

    fn with(origin: Option<&str>) -> impl FnOnce(&str) -> Result<Option<DatasetView>, String> {
        let v = view(origin);
        move |_| Ok(Some(v))
    }

    fn none(_: &str) -> Result<Option<DatasetView>, String> {
        Ok(None)
    }

    #[test]
    fn regions_resolve_to_one_key_and_source() {
        let r = resolve("denmark", none).unwrap();
        assert_eq!(r.key, "denmark");
        assert_eq!(r.source, Source::Geofabrik("europe/denmark".into()));
        let r = resolve("asia/japan/kanto/", none).unwrap();
        assert_eq!(r.key, "kanto");
        assert_eq!(r.source.origin(), "download.geofabrik.de/asia/japan/kanto");
        assert_eq!(
            r.source.pbf_url(),
            "https://download.geofabrik.de/asia/japan/kanto-latest.osm.pbf"
        );
        assert_eq!(resolve("planet", none).unwrap().source, Source::Planet);
        assert!(resolve("atlantis", none).is_err());
        assert!(resolve("a/b c", none).is_err());
    }

    #[test]
    fn a_registered_key_carries_its_recorded_source() {
        let r = resolve("world", with(Some(PLANET))).unwrap();
        assert_eq!((r.key.as_str(), r.source), ("world", Source::Planet));
        let r = resolve("denmark", with(Some("Geofabrik"))).unwrap();
        assert_eq!(r.source, Source::Geofabrik("europe/denmark".into()));
        assert!(resolve("fiji", with(Some("Geofabrik"))).is_err());
        assert!(resolve("fiji", with(None)).is_err());
    }

    #[test]
    fn a_key_never_changes_source() {
        let georgia = Some("download.geofabrik.de/asia/georgia");
        assert!(resolve("asia/georgia", with(georgia)).is_ok());
        assert!(resolve("north-america/us/georgia", with(georgia)).is_err());
        assert!(resolve("denmark", with(Some(PLANET))).is_err());
    }
}

#[cfg(test)]
mod osc_tests {
    use super::*;

    #[test]
    fn replication_urls() {
        assert_eq!(seq_path(4715), "000/004/715");
        assert_eq!(seq_path(0), "000/000/000");
        assert_eq!(seq_path(123_456_789), "123/456/789");
        let region = Source::Geofabrik("europe/denmark".into());
        assert_eq!(
            region.osc_url(4715),
            "https://download.geofabrik.de/europe/denmark-updates/000/004/715.osc.gz"
        );
        assert_eq!(
            Source::Planet.osc_url(4715),
            "https://planet.openstreetmap.org/replication/day/000/004/715.osc.gz"
        );
    }
}
