//! The one "node, way or relation" type.

use std::str::FromStr;

/// An OSM element kind.
///
/// Declaration order is the OSM type-then-id order every sorted PBF uses:
/// node < way < relation. `Ord`, [`ordinal`](Self::ordinal) and the derived
/// comparison all follow it; the `order_and_spellings` unit test pins it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ElementKind {
    /// A node.
    Node,
    /// A way.
    Way,
    /// A relation.
    Relation,
}

impl ElementKind {
    /// The lowercase OSM name: `"node"`, `"way"` or `"relation"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Way => "way",
            Self::Relation => "relation",
        }
    }

    /// Position in the OSM type-then-id order: 0, 1 or 2.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Node => 0,
            Self::Way => 1,
            Self::Relation => 2,
        }
    }

    /// The one-letter prefix used in id specs: `'n'`, `'w'` or `'r'`.
    #[must_use]
    pub const fn type_char(self) -> char {
        match self {
            Self::Node => 'n',
            Self::Way => 'w',
            Self::Relation => 'r',
        }
    }

    /// The inverse of [`as_str`](Self::as_str): exactly `node`, `way` or
    /// `relation`, nothing else. Used where a file format fixes the spelling
    /// (OSC element names, relation member types).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "node" => Some(Self::Node),
            "way" => Some(Self::Way),
            "relation" => Some(Self::Relation),
            _ => None,
        }
    }

    /// The inverse of [`ordinal`](Self::ordinal).
    #[must_use]
    pub const fn from_ordinal(ordinal: u8) -> Option<Self> {
        match ordinal {
            0 => Some(Self::Node),
            1 => Some(Self::Way),
            2 => Some(Self::Relation),
            _ => None,
        }
    }
}

impl std::fmt::Display for ElementKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ElementKind {
    type Err = crate::Error;

    /// Accepts the full names `node`, `way`, `relation` and the one-letter
    /// forms `n`, `w`, `r`. Nothing else, no case folding. Anything else is
    /// an `InvalidArgument` error.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "n" => Ok(Self::Node),
            "w" => Ok(Self::Way),
            "r" => Ok(Self::Relation),
            other => Self::from_name(other).ok_or_else(|| {
                crate::Error::invalid_argument(format!(
                    "invalid element kind '{other}': expected node, way or relation"
                ))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ElementKind;

    #[test]
    fn order_and_spellings() {
        let all = [ElementKind::Node, ElementKind::Way, ElementKind::Relation];
        assert!(ElementKind::Node < ElementKind::Way);
        assert!(ElementKind::Way < ElementKind::Relation);
        assert_eq!(size_of::<ElementKind>(), 1);
        for (i, kind) in all.into_iter().enumerate() {
            assert_eq!(usize::from(kind.ordinal()), i);
            assert_eq!(ElementKind::from_ordinal(kind.ordinal()), Some(kind));
        }
        assert_eq!(ElementKind::from_ordinal(3), None);
        assert_eq!(ElementKind::Node.as_str(), "node");
        assert_eq!(ElementKind::Way.as_str(), "way");
        assert_eq!(ElementKind::Relation.as_str(), "relation");
        assert_eq!(ElementKind::Node.type_char(), 'n');
        assert_eq!(ElementKind::Way.type_char(), 'w');
        assert_eq!(ElementKind::Relation.type_char(), 'r');
        for kind in all {
            assert_eq!(kind.as_str().parse::<ElementKind>().ok(), Some(kind));
            assert_eq!(
                kind.type_char().to_string().parse::<ElementKind>().ok(),
                Some(kind)
            );
        }
        assert!("Node".parse::<ElementKind>().is_err());
        assert!("".parse::<ElementKind>().is_err());
    }
}
