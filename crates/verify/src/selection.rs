//! Verifier policy over devkit's policy-free selections.
use crate::{Source, eligible, error::Error};
use clap::Args;
use pbfhogg_devkit::{Bbox, Config, ResolvedInput};
use std::path::{Path, PathBuf};

/// OSC selection options shared by change-consuming checks and the suite.
#[derive(Args, Debug, Clone, Default)]
pub(crate) struct Selections {
    /// Select a configured OSC sequence (otherwise require exactly one).
    #[arg(long, conflicts_with = "osc")]
    pub(crate) osc_seq: Option<String>,
    /// Fixture OSC instead of a dataset-resolved change file.
    #[arg(long)]
    pub(crate) osc: Option<PathBuf>,
}

pub(crate) struct Selection<'a> {
    pub(crate) config: &'a Config,
    pub(crate) host: &'a str,
    pub(crate) source: &'a Source,
}

pub(crate) fn fixture(config: &Config, path: &Path) -> Result<PathBuf, Error> {
    let path = config.project_root().join(path);
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| Error::tool(format!("fixture {}: {e}", path.display())))?;
    if !std::fs::metadata(&canonical)?.is_file() {
        return Err(Error::tool(format!(
            "fixture {} is not a regular file",
            path.display()
        )));
    }
    Ok(canonical)
}

impl Selection<'_> {
    /// Fallback is verifier policy, never devkit or bench policy. Only an
    /// empty snapshot table falls back, never an absent requested sequence.
    pub(crate) fn osc(&self, args: &Selections) -> Result<Option<PathBuf>, Error> {
        if let Some(path) = &args.osc {
            return fixture(self.config, path).map(Some);
        }
        eligible(&self.source.dataset)?;
        let snapshot = self.source.snapshot.as_deref();
        let fallback = snapshot.is_some_and(|s| s != "base")
            && !self
                .config
                .has_osc(self.host, &self.source.dataset, snapshot)?;
        let selected = if fallback {
            crate::output::note(&format!(
                "snapshot '{}' has no OSC table; using primary dataset '{}' OSC table",
                snapshot.unwrap_or_default(),
                self.source.dataset
            ));
            None
        } else {
            snapshot
        };
        Ok(self
            .config
            .select_osc(
                self.host,
                &self.source.dataset,
                selected,
                args.osc_seq.as_deref(),
            )?
            .map(|input| input.path))
    }

    pub(crate) fn osc_range(&self, range: &str) -> Result<Vec<PathBuf>, Error> {
        let (lo, hi) = range
            .split_once("..")
            .ok_or_else(|| Error::tool("OSC range must be LO..HI"))?;
        let lo = lo
            .parse::<u64>()
            .map_err(|_| Error::tool("invalid OSC range start"))?;
        let hi = hi
            .parse::<u64>()
            .map_err(|_| Error::tool("invalid OSC range end"))?;
        eligible(&self.source.dataset)?;
        let snapshot = self.source.snapshot.as_deref();
        let fallback = snapshot.is_some_and(|s| s != "base")
            && !self
                .config
                .has_osc(self.host, &self.source.dataset, snapshot)?;
        if fallback {
            crate::output::note("snapshot has no OSC table; using primary dataset OSC table");
        }
        Ok(self
            .config
            .select_osc_range(
                self.host,
                &self.source.dataset,
                if fallback { None } else { snapshot },
                lo,
                hi,
            )?
            .into_iter()
            .map(|i| i.path)
            .collect())
    }

    pub(crate) fn bbox(&self, bbox: Option<&Bbox>) -> Result<Option<Bbox>, Error> {
        if let Some(bbox) = bbox {
            return Ok(Some(bbox.clone()));
        }
        eligible(&self.source.dataset)?;
        Ok(self.config.select_bbox(self.host, &self.source.dataset)?)
    }

    /// Use the selected generation, never substitute a primary PBF for a
    /// snapshot. A configured invalid keep-untagged base must fail.
    pub(crate) fn locations_base(&self) -> Result<Option<ResolvedInput>, Error> {
        if self.source.input.is_some() {
            return Ok(None);
        }
        eligible(&self.source.dataset)?;
        Ok(self.config.select_pbf(
            self.host,
            &self.source.dataset,
            self.source.snapshot.as_deref(),
            "altw-keep-untagged",
        )?)
    }

    pub(crate) fn polygons(&self) -> Result<Option<Vec<ResolvedInput>>, Error> {
        eligible(&self.source.dataset)?;
        Ok(self
            .config
            .select_polygons(self.host, &self.source.dataset)?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[allow(
        clippy::too_many_lines,
        reason = "Keep selection policy cases together for audit"
    )]
    #[test]
    fn verifier_selections_apply_fallback_and_eligibility_per_input() {
        let scratch = pbfhogg_test_scratch::scratch();
        let root = scratch.path();
        std::fs::create_dir(root.join("data")).unwrap();
        for name in ["base.osc", "snap.osc", "base.pbf", "fixture.osc"] {
            std::fs::write(root.join("data").join(name), name).unwrap();
        }
        std::fs::write(
            root.join("brokkr.toml"),
            "
[m.datasets.denmark]
bbox = '1,2,3,4'
[m.datasets.denmark.osc.10]
file = 'base.osc'
[m.datasets.denmark.pbf.altw-keep-untagged]
file = 'base.pbf'
[m.datasets.denmark.snapshot.empty]
[m.datasets.denmark.snapshot.own.osc.11]
file = 'snap.osc'
[m.datasets.denmark.snapshot.bad.osc.10]
file = 'missing.osc'
[m.datasets.denmark.snapshot.invalid.osc]
10 = 3
[m.datasets.denmark.snapshot.broken-base.pbf.altw-keep-untagged]
file = 'missing.pbf'
[m.datasets.germany]

",
        )
        .unwrap();
        let config = Config::load(&root.join("brokkr.toml"), None).unwrap();
        let mut source = Source {
            dataset: "denmark".into(),
            variant: "indexed".into(),
            snapshot: Some("empty".into()),
            input: None,
        };
        let args = Selections::default();
        fn select<'a>(config: &'a Config, source: &'a Source) -> Selection<'a> {
            Selection {
                config,
                host: "m",
                source,
            }
        }
        assert!(
            select(&config, &source)
                .osc(&args)
                .unwrap()
                .unwrap()
                .ends_with("base.osc")
        );
        assert!(select(&config, &source).locations_base().unwrap().is_none());
        assert!(select(&config, &source).osc_range("10..10").unwrap()[0].ends_with("base.osc"));
        assert!(select(&config, &source).osc_range("10..11").is_err());
        assert!(select(&config, &source).osc_range("11..10").is_err());
        assert!(select(&config, &source).osc_range("invalid").is_err());
        source.snapshot = Some("own".into());
        assert!(select(&config, &source).osc_range("10..11").is_err());
        assert!(select(&config, &source).osc_range("11..11").unwrap()[0].ends_with("snap.osc"));
        assert!(
            select(&config, &source)
                .osc(&args)
                .unwrap()
                .unwrap()
                .ends_with("snap.osc")
        );
        let explicit = Selections {
            osc_seq: Some("10".into()),
            ..Default::default()
        };
        assert!(select(&config, &source).osc(&explicit).is_err());
        for snapshot in ["bad", "invalid", "missing"] {
            source.snapshot = Some(snapshot.into());
            assert!(select(&config, &source).osc(&args).is_err());
        }
        source.snapshot = Some("broken-base".into());
        assert!(select(&config, &source).locations_base().is_err());
        source.input = Some("fixture.pbf".into());
        assert!(select(&config, &source).locations_base().unwrap().is_none());
        source.input = None;
        source.snapshot = None;
        assert!(select(&config, &source).locations_base().unwrap().is_some());
        assert_eq!(
            select(&config, &source)
                .bbox(None)
                .unwrap()
                .unwrap()
                .coordinates(),
            [1.0, 2.0, 3.0, 4.0]
        );
        source.dataset = "germany".into();
        assert!(select(&config, &source).osc(&args).unwrap().is_none());
        assert!(select(&config, &source).osc(&explicit).is_err());
        assert!(select(&config, &source).locations_base().unwrap().is_none());
        source.input = Some("fixture.pbf".into());
        assert!(select(&config, &source).locations_base().unwrap().is_none());
        source.dataset = "planet".into();
        assert!(select(&config, &source).osc(&args).is_err());
        assert!(select(&config, &source).bbox(None).is_err());
        assert!(select(&config, &source).polygons().is_err());
        let fixture_args = Selections {
            osc: Some("data/fixture.osc".into()),
            ..Default::default()
        };
        assert!(
            select(&config, &source)
                .osc(&fixture_args)
                .unwrap()
                .is_some()
        );
        assert!(
            select(&config, &source)
                .bbox(Some(&Bbox::parse("-1,2,3,4").unwrap()))
                .unwrap()
                .is_some()
        );
        source.input = None;
        assert!(select(&config, &source).locations_base().is_err());
    }
}
