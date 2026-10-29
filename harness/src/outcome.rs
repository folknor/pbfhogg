//! A call's outcome as its document reports it.

use pbfhogg::ErrorKind;
use serde_json::{Value, json};

/// The protocol's `kind` for an error: a fixed name per kind, `other` for a
/// kind added after this list (`ErrorKind` is non-exhaustive).
fn kind_id(kind: &ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Io(_) => "io",
        ErrorKind::PlatformUnsupported { .. } => "platform_unsupported",
        ErrorKind::StringtableUtf8 { .. } => "stringtable_utf8",
        ErrorKind::StringtableIndexOutOfBounds { .. } => "stringtable_index_out_of_bounds",
        ErrorKind::Blob(_) => "blob",
        ErrorKind::WireFormat { .. } => "wire_format",
        ErrorKind::MissingHeader => "missing_header",
        ErrorKind::MissingFeature(_) => "missing_feature",
        ErrorKind::MissingIndexdata { .. } => "missing_indexdata",
        ErrorKind::Unsorted { .. } => "unsorted",
        ErrorKind::IndexdataMismatch { .. } => "indexdata_mismatch",
        ErrorKind::MissingNodeLocations { .. } => "missing_node_locations",
        ErrorKind::InputReplaced => "input_replaced",
        ErrorKind::BlobTooLarge { .. } => "blob_too_large",
        ErrorKind::VersionRegression { .. } => "version_regression",
        ErrorKind::InvalidArgument { .. } => "invalid_argument",
        ErrorKind::InvalidData { .. } => "invalid_data",
        ErrorKind::Internal { .. } => "internal",
        ErrorKind::External { .. } => "external",
        ErrorKind::Output(_) => "output",
        ErrorKind::Cancelled => "cancelled",
        _ => "other",
    }
}

/// How a call ended.
pub(crate) enum CallOutcome {
    Ok,
    Cancelled,
    Error { kind: &'static str, message: String },
}

impl CallOutcome {
    pub(crate) fn of(result: &pbfhogg::Result<()>) -> Self {
        match result {
            Ok(()) => Self::Ok,
            Err(error) if error.is_cancelled() => Self::Cancelled,
            Err(error) => Self::Error {
                kind: kind_id(error.kind()),
                message: crate::protocol::one_line(&error.report(), crate::protocol::MESSAGE_MAX),
            },
        }
    }

    pub(crate) fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Ok => json!({ "status": "ok" }),
            Self::Cancelled => json!({ "status": "cancelled" }),
            Self::Error { kind, message } => {
                json!({ "status": "error", "kind": kind, "message": message })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CallOutcome;

    #[test]
    fn an_error_reports_its_kind_and_one_line() {
        // A call id the wire refuses is the library's InvalidArgument.
        let error = pbfhogg::debug::CallId::new("a b").expect_err("refused");
        let outcome = CallOutcome::of(&Err(error)).to_json();
        assert_eq!(outcome["status"], "error");
        assert_eq!(outcome["kind"], "invalid_argument");
        let message = outcome["message"].as_str().expect("a string");
        assert!(!message.is_empty() && !message.chars().any(char::is_control));
    }
}
