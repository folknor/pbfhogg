use std::collections::HashSet;

use crate::{DenseNodeInfo, Info};

use super::ExportOptions;

pub(super) struct MetaView<'a> {
    version: Option<i32>,
    timestamp_millis: Option<i64>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user: Option<crate::Result<&'a str>>,
    visible: Option<bool>,
}

impl<'a> MetaView<'a> {
    pub(super) fn from_info(info: &Info<'a>) -> Self {
        Self {
            version: info.version(),
            timestamp_millis: info.milli_timestamp(),
            changeset: info.changeset(),
            uid: info.uid(),
            user: info.user(),
            visible: info.visible_opt(),
        }
    }

    pub(super) fn from_dense(info: &'a DenseNodeInfo<'a>) -> Self {
        Self {
            version: Some(info.version()),
            timestamp_millis: Some(info.milli_timestamp()),
            changeset: Some(info.changeset()),
            uid: Some(info.uid()),
            user: Some(info.user()),
            visible: Some(info.visible()),
        }
    }
}

fn push_json_string(buf: &mut String, value: &str) -> crate::Result<()> {
    let encoded =
        serde_json::to_string(value).map_err(|error| std::io::Error::other(error.to_string()))?;
    buf.push_str(&encoded);
    Ok(())
}

fn push_name(buf: &mut String, first: &mut bool, name: &str) -> crate::Result<()> {
    if !*first {
        buf.push(',');
    }
    *first = false;
    push_json_string(buf, name)?;
    buf.push(':');
    Ok(())
}

pub(super) fn write_properties<'a, T>(
    buf: &mut String,
    id: i64,
    object_type: &str,
    tags: T,
    meta: Option<&MetaView<'a>>,
    opts: &ExportOptions,
) -> crate::Result<()>
where
    T: Iterator<Item = (&'a str, &'a str)>,
{
    use std::fmt::Write;

    buf.clear();
    buf.push('{');
    let mut first = true;
    push_name(buf, &mut first, "@id")?;
    write!(buf, "{id}").ok();
    push_name(buf, &mut first, "@type")?;
    push_json_string(buf, object_type)?;

    let mut reserved = HashSet::from(["@id", "@type"]);
    if opts.metadata {
        reserved.extend([
            "@version",
            "@timestamp",
            "@changeset",
            "@uid",
            "@user",
            "@visible",
        ]);
        if let Some(meta) = meta {
            if let Some(value) = meta.version {
                push_name(buf, &mut first, "@version")?;
                write!(buf, "{value}").ok();
            }
            if let Some(millis) = meta.timestamp_millis {
                push_name(buf, &mut first, "@timestamp")?;
                let seconds = millis.div_euclid(1000).cast_unsigned();
                push_json_string(buf, &crate::commands::format_epoch_secs(seconds))?;
            }
            if let Some(value) = meta.changeset {
                push_name(buf, &mut first, "@changeset")?;
                write!(buf, "{value}").ok();
            }
            if let Some(value) = meta.uid {
                push_name(buf, &mut first, "@uid")?;
                write!(buf, "{value}").ok();
            }
            if let Some(user) = &meta.user {
                push_name(buf, &mut first, "@user")?;
                push_json_string(
                    buf,
                    user.as_ref()
                        .map_err(|error| std::io::Error::other(error.to_string()))?,
                )?;
            }
            if let Some(value) = meta.visible {
                push_name(buf, &mut first, "@visible")?;
                buf.push_str(if value { "true" } else { "false" });
            }
        }
    }

    let mut emitted = HashSet::new();
    for (key, value) in tags {
        if reserved.contains(key)
            || !emitted.insert(key)
            || opts
                .properties
                .as_ref()
                .is_some_and(|keys| !keys.contains(key))
        {
            continue;
        }
        push_name(buf, &mut first, key)?;
        push_json_string(buf, value)?;
    }
    buf.push('}');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::export::{ExportFormat, ExportTypes};

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
        let mut buf = String::new();
        write_properties(&mut buf, 7, "node", tags.into_iter(), None, &options)
            .expect("properties");
        assert_eq!(
            buf,
            "{\"@id\":7,\"@type\":\"node\",\"name\":\"a \\\"quote\\\" and \\\\ slash\"}"
        );
    }
}
