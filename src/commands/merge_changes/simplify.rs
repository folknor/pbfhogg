//! `merge-changes --simplify`: the last change per object, on the streaming
//! path's readers.
//!
//! # Shape
//!
//! 1. **Render.** The default path's readers lex every input into batches
//!    ([`stream::collect`]); pool workers replay each batch into an indexed
//!    [`ByteSink`], writing plain XML and recording, per element, its kind,
//!    ID, action, version claim and byte range. Every input is read and
//!    checked to its end before anything below runs, so an input failure
//!    always outranks a version regression.
//! 2. **Dedupe.** One key per change, `(kind, id, batch, element)`, where
//!    batches are numbered in input then batch order: the key's position
//!    part is the change's place in the concatenated input. A parallel sort
//!    groups each object's changes in that order. Per group the first claim
//!    seeds the [`VersionFloor`] and every later claim is admitted, then
//!    raised, exactly as a serial pass in input order would; the group's
//!    last change survives. When several objects regress, the one at the
//!    smallest position is reported: the one a serial pass meets first.
//!    With `--allow-version-regression` no floor is kept.
//! 3. **Emit.** Survivors come out in `(kind, id)` order, split into
//!    create, modify and delete groups. Each group is cut into
//!    `len.div_ceil(workers)` chunks; a chunk is one `<action>` section and,
//!    under gzip, one member. The surviving elements' rendered bytes are
//!    spliced into the section, so the output is byte for byte what writing
//!    owned elements produced (a differential test holds it to that).
//!
//! # Memory
//!
//! The rendered XML of every input change plus a 40-byte record and 24-byte
//! key per change, instead of every change as owned strings and vectors. A
//! superseded change's bytes stay in their batch until the run ends. The
//! output bytes depend on the worker count (and through `-j 0` on the
//! host's core count) while the changes and their order do not.

use quick_xml::events::{BytesEnd, BytesStart, Event};
use rayon::prelude::*;

use super::emit::{ByteSink, Element};
use super::stream;
use super::{Action, OscWriter, action_tag, build_postlude_bytes, build_prelude_bytes};
use crate::ElementKind;
use crate::osc::version_floor::VersionFloor;
use crate::osc::xml_parse::LexBatch;

/// One rendered batch: plain XML and the elements written into it.
struct Rendered {
    xml: Vec<u8>,
    elements: Vec<Element>,
}

impl Rendered {
    fn bytes(&self, element: &Element) -> &[u8] {
        &self.xml[element.start..element.end]
    }
}

fn render_batch(batch: &LexBatch, path: &std::path::Path) -> crate::Result<Rendered> {
    let mut writer = OscWriter::new_buf(false);
    let mut open_action: Option<Action> = None;
    let mut count = 0u64;
    let mut elements = Vec::new();
    {
        let mut sink =
            ByteSink::with_index(&mut writer, &mut open_action, &mut count, &mut elements);
        batch.replay(&mut sink, path)?;
    }
    // The open action section is never closed: only element ranges are read.
    Ok(Rendered {
        xml: writer.writer.into_inner(),
        elements,
    })
}

/// A change's sort key: object, then place in the concatenated input.
type Key = (ElementKind, i64, u32, u32);

/// Returns `(changes in, changes out)`.
pub(super) fn run(
    plan: &stream::Plan<'_>,
    output: &crate::output::BoundOutput,
    worker_count: usize,
    allow_version_regression: bool,
) -> crate::Result<(u64, u64)> {
    let cancel = plan.cancel;
    for path in plan.inputs {
        if let Ok(meta) = std::fs::metadata(path) {
            cancel.counter("merge_changes_input_bytes", meta.len());
        }
    }

    cancel.marker("MERGECHANGES_PARALLEL_PARSE_START");
    let per_input = stream::collect(plan, &render_batch)?;
    let mut batches: Vec<Rendered> = Vec::new();
    for input in per_input {
        let changes: usize = input.iter().map(|b| b.elements.len()).sum();
        cancel.counter("merge_changes_changes_per_osc", changes);
        batches.extend(input);
    }
    cancel.marker("MERGECHANGES_PARALLEL_PARSE_END");
    cancel.check()?;

    cancel.marker("MERGECHANGES_SIMPLIFY_START");
    let changes_in: usize = batches.iter().map(|b| b.elements.len()).sum();
    cancel.counter("merge_changes_changes_in", changes_in);
    let groups = dedupe(&batches, changes_in, allow_version_regression)?;
    cancel.marker("MERGECHANGES_SIMPLIFY_END");
    // Stage boundary: the sort and scan above have no checkpoint of their own.
    cancel.check()?;

    let changes_out = groups.iter().map(Vec::len).sum::<usize>();
    cancel.marker("MERGECHANGES_PARALLEL_EMIT_START");
    let mut all_chunks: Vec<Vec<u8>> = Vec::new();
    for (action, group) in [Action::Create, Action::Modify, Action::Delete]
        .into_iter()
        .zip(&groups)
    {
        if group.is_empty() {
            continue;
        }
        let chunk_len = group.len().div_ceil(worker_count).max(1);
        let chunks: crate::Result<Vec<Vec<u8>>> = group
            .par_chunks(chunk_len)
            .map(|chunk| -> crate::Result<Vec<u8>> {
                let mut writer = OscWriter::new_buf(plan.is_gz);
                writer.write_event(Event::Start(BytesStart::new(action_tag(action))))?;
                // Raw bytes, not events: an element leaves the writer's indent
                // state as it found it, so the closing tag below indents as it
                // would after writing the elements one event at a time.
                let buf = writer.writer.get_mut();
                for &(batch, element) in chunk {
                    let rendered = &batches[batch as usize];
                    buf.extend_from_slice(rendered.bytes(&rendered.elements[element as usize]));
                }
                writer.write_event(Event::End(BytesEnd::new(action_tag(action))))?;
                writer.into_inner()
            })
            .collect();
        all_chunks.extend(chunks?);
        cancel.check()?;
    }
    drop(batches);
    cancel.marker("MERGECHANGES_PARALLEL_EMIT_END");

    cancel.marker("MERGECHANGES_DRAIN_START");
    let mut out = output.writer()?;
    std::io::Write::write_all(&mut out, &build_prelude_bytes(plan.is_gz)?)?;
    for bytes in &all_chunks {
        std::io::Write::write_all(&mut out, bytes)?;
    }
    std::io::Write::write_all(&mut out, &build_postlude_bytes(plan.is_gz)?)?;
    out.finish()?;
    cancel.marker("MERGECHANGES_DRAIN_END");
    cancel.marker("MERGECHANGES_WRITE_FINISH_START");
    cancel.marker("MERGECHANGES_WRITE_FINISH_END");

    Ok((changes_in as u64, changes_out as u64))
}

/// Each object's surviving change as `(batch, element)`, in `(kind, id)`
/// order, grouped create, modify, delete.
fn dedupe(
    batches: &[Rendered],
    changes_in: usize,
    allow_version_regression: bool,
) -> crate::Result<[Vec<(u32, u32)>; 3]> {
    let mut keys: Vec<Key> = Vec::with_capacity(changes_in);
    for (b, rendered) in batches.iter().enumerate() {
        let b = position(b)?;
        for (i, e) in rendered.elements.iter().enumerate() {
            keys.push((e.kind, e.id, b, position(i)?));
        }
    }
    keys.par_sort_unstable();
    let element = |k: &Key| &batches[k.2 as usize].elements[k.3 as usize];

    let mut groups: [Vec<(u32, u32)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    // The regression a serial pass in input order would meet first.
    let mut first_regression: Option<(Key, i32, i32)> = None;
    for object in keys.chunk_by(|a, b| a.0 == b.0 && a.1 == b.1) {
        if !allow_version_regression {
            let mut floor = VersionFloor::default();
            let mut claims = object.iter();
            if let Some(first) = claims.next() {
                floor.raise(element(first).version);
            }
            for key in claims {
                let claim = element(key).version;
                if let Some(version) = claim
                    && let Err(held) = floor.admit(claim)
                {
                    let earlier =
                        first_regression.is_none_or(|(at, _, _)| (key.2, key.3) < (at.2, at.3));
                    if earlier {
                        first_regression = Some((*key, version, held));
                    }
                    break;
                }
                floor.raise(claim);
            }
        }
        let Some(last) = object.last() else {
            continue;
        };
        let slot = match element(last).action {
            Action::Create => 0,
            Action::Modify => 1,
            Action::Delete => 2,
        };
        groups[slot].push((last.2, last.3));
    }
    if let Some((key, version, held)) = first_regression {
        return Err(crate::error::new_version_regression(
            "merge-changes simplification",
            key.0,
            key.1,
            version,
            held,
        ));
    }
    Ok(groups)
}

fn position(i: usize) -> crate::Result<u32> {
    u32::try_from(i)
        .map_err(|_| crate::Error::internal("merge-changes: more than 4 Gi batches or elements"))
}
