//! Pass 1.5: way extraction and referenced-node collection.
//!
//! The one read of the way blobs. Each is validated as block construction
//! validates it; its geocode-relevant ways (streets, building addresses,
//! interpolation, admin members) are written to the way store
//! (`super::way_store`), which Pass 2b resolves from, and the node IDs they
//! reference are collected. The coordinate store in pass 2 only holds
//! entries for these nodes, so its size follows the referenced subset
//! rather than every node in the file.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::Result;
use crate::error::Error;
use crate::read::input::{InputFile, PinnedInput};
use crate::read::project_ids::Span;
use crate::scan::classify::ScheduledBlob;

use crate::tunables::{NODE_PREALLOC_IDS_PER_BYTE, OVERFLOW_COMPACT_MIN};

/// Record one accepted node ref: through `set_atomic` when it falls inside
/// the pre-allocated range, otherwise onto this worker's overflow list.
#[inline]
fn record_referenced_node(
    referenced_nodes: &crate::idset::IdSet,
    overflow: &mut Vec<i64>,
    prealloc_max: i64,
    node_id: i64,
) {
    if node_id > prealloc_max {
        overflow.push(node_id);
    } else {
        referenced_nodes.set_atomic(node_id);
    }
}

/// Sort and deduplicate a worker's overflow list once it has doubled since
/// the last compaction (`compacted_len`, updated here).
fn compact_overflow_nodes(overflow: &mut Vec<i64>, compacted_len: &mut usize) {
    if overflow.len() < OVERFLOW_COMPACT_MIN || overflow.len() < compacted_len.saturating_mul(2) {
        return;
    }
    overflow.sort_unstable();
    overflow.dedup();
    *compacted_len = overflow.len();
}

/// Insert the workers' overflow lists after the parallel scan, where `set`
/// may grow the set under `&mut`.
fn insert_overflow_nodes(referenced_nodes: &mut crate::idset::IdSet, lists: Vec<Vec<i64>>) {
    for list in lists {
        for node_id in list {
            referenced_nodes.set(node_id);
        }
    }
}

/// Record the accepted refs of one geocode-relevant way.
fn record_way_refs(
    referenced: &crate::idset::IdSet,
    overflow_nodes: &mut Vec<i64>,
    prealloc_max: i64,
    max_node_id: i64,
    refs: &[i64],
    negative_node_ids: &mut Vec<i64>,
) -> u64 {
    let mut negative_count = 0_u64;
    for &r in refs {
        // Only refs in `0..=max_node_id`
        // can name a node Phase 2a will
        // see. Negative refs are skipped
        // as non-atomic `IdSet::set` does.
        // A ref above the claimed bound is a
        // dangling ref on honest input (a
        // simple-strategy extract keeps
        // ways whose nodes lie outside it,
        // with IDs that can exceed every
        // node present): no coordinate can
        // ever be written for it, so
        // dropping it here changes nothing
        // downstream. That holds only while
        // the bound is true, which is why
        // Phase 2a errors on any decoded
        // node above it. Accepted refs above
        // the size-bounded dense range go to
        // this worker's overflow list instead
        // of extending `pre_allocate`.
        if r < 0 {
            negative_count = negative_count.saturating_add(1);
            super::sample_id(negative_node_ids, r);
            continue;
        }
        if r > max_node_id {
            continue;
        }
        record_referenced_node(referenced, overflow_nodes, prealloc_max, r);
    }
    negative_count
}

/// Whether any worker has already reported an error.
///
/// Workers call this before every claim from the shared way-blob counter,
/// so it locks the first-error mutex once per way blob. The altw external
/// stage 2 and stage 3 bucket pools do the same once per bucket.
fn has_first_err(slot: &Mutex<Option<Error>>) -> bool {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some()
}

/// Keep the first error any worker reports.
fn store_first_err(slot: &Mutex<Option<Error>>, e: Error) {
    let mut slot = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(e);
    }
}

/// Hand a finished worker's overflow list to the shared collection.
fn push_overflow_list(lists: &Mutex<Vec<Vec<i64>>>, overflow_nodes: Vec<i64>) {
    if !overflow_nodes.is_empty() {
        lists
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(overflow_nodes);
    }
}

/// Fold one worker's negative-ref tally and ID sample into the shared ones,
/// once per worker after its scan loop.
fn publish_negative_node_refs(
    shared_count: &AtomicU64,
    shared_sample: &Mutex<Vec<i64>>,
    count: u64,
    sample: Vec<i64>,
) {
    shared_count.fetch_add(count, Ordering::Relaxed);
    let mut shared_sample = shared_sample
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for id in sample {
        super::sample_id(&mut shared_sample, id);
    }
}

/// Emit the negative-ref counter and, when any were skipped, a warning with
/// the bounded ID sample.
fn report_negative_node_refs(
    count: &AtomicU64,
    sample: Mutex<Vec<i64>>,
    host: &crate::host::Entered<'_>,
) {
    let negative_count = count.load(Ordering::Relaxed);
    host.invocation
        .counter("geocode_negative_node_refs", negative_count);
    if negative_count > 0 {
        let sample = sample
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::host::diag!(
            host.diagnostics,
            Warning,
            "geocode.pass1_5.negative_node_refs",
            "skipped {negative_count} negative node refs; node IDs (sample): {sample:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Way extraction: classification and record encoding
// ---------------------------------------------------------------------------

/// What a way is to the geocode index, from its tags. Admin membership is
/// by way ID and is tested beside it.
struct WayGeocodeFlags {
    is_street: bool,
    is_building_addr: bool,
    is_interp: bool,
}

impl WayGeocodeFlags {
    /// From the presence of each key and whether the highway value is one
    /// of `GEOCODE_EXCLUDED_HIGHWAYS`.
    ///
    /// An interpolation way needs only `addr:interpolation`: OSM puts the
    /// street and numbers on its end nodes, and the way "doesn't take
    /// addr:street=*" (wiki, Addresses, Interpolation), so requiring the
    /// street on the way would drop exactly the conforming ways. A way that
    /// also carries `addr:housenumber` is an addressed object, not a range
    /// (Nominatim excludes it the same way), and is classified as a building
    /// address when it qualifies as one.
    ///
    /// A building address names an `addr:street` or an `addr:place` (OSM's
    /// tag for an address that belongs to a place, not a street). This is
    /// presence only: Pass 2b then drops one whose names are all empty, so
    /// Pass 1.5 collects at most a few refs more than Pass 2b uses.
    #[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
    fn classify(
        highway: bool,
        name: bool,
        excluded: bool,
        building: bool,
        housenumber: bool,
        street: bool,
        place: bool,
        interpolation: bool,
    ) -> Self {
        Self {
            is_street: highway && name && !excluded,
            is_building_addr: building && housenumber && (street || place),
            is_interp: interpolation && !housenumber,
        }
    }
}

/// A way's ID and the positions of its tag and ref arrays, recorded by the
/// validator's way hook and read back only once the blob is accepted
/// (`read::project_ids` states the convention).
struct WayTags {
    id: i64,
    keys: Span,
    vals: Span,
    refs: Span,
}

/// The projection riding on the block validator: every way's spans, in the
/// validator's way order (the element walk's).
struct WayTagSpans<'s> {
    ways: &'s mut Vec<WayTags>,
}

impl crate::read::block::GroupProjection for WayTagSpans<'_> {
    fn way(
        &mut self,
        way: &crate::read::wire::WireWay<'_>,
        _info: Option<&crate::read::wire::WireInfo>,
    ) {
        self.ways.push(WayTags {
            id: way.id,
            keys: Span::of(way.keys_data),
            vals: Span::of(way.vals_data),
            refs: Span::of(way.refs_data),
        });
    }
}

// Per-string classes, cached per blob by string-table index. Zero is
// "not yet classified"; the low nibble is the key, `CLASS_EXCLUDED_HIGHWAY`
// marks a value that excludes a highway from the street set.
const CLASS_UNSEEN: u8 = 0;
const CLASS_OTHER: u8 = 1;
const CLASS_HIGHWAY: u8 = 2;
const CLASS_NAME: u8 = 3;
const CLASS_HOUSENUMBER: u8 = 4;
const CLASS_STREET: u8 = 5;
const CLASS_PLACE: u8 = 6;
const CLASS_POSTCODE: u8 = 7;
const CLASS_BUILDING: u8 = 8;
const CLASS_INTERPOLATION: u8 = 9;
const CLASS_KEY_MASK: u8 = 0x0f;
const CLASS_EXCLUDED_HIGHWAY: u8 = 0x10;

/// One accepted blob's string table, read as bytes (its UTF-8 was checked
/// by the validator before these are read).
struct BlobStrings<'b> {
    bytes: &'b [u8],
    table: &'b [(u32, u32)],
}

impl<'b> BlobStrings<'b> {
    fn get(&self, index: u32) -> Result<&'b [u8]> {
        let &(offset, len) = self
            .table
            .get(index as usize)
            .ok_or_else(string_index_err)?;
        let start = offset as usize;
        self.bytes
            .get(start..start + len as usize)
            .ok_or_else(|| Error::internal("validated string table entry lies outside its blob"))
    }

    /// The class of string `index`, by byte equality: every duplicate of a
    /// literal in the table classifies as the literal, as the decoded tags
    /// compare.
    fn class(&self, classes: &mut [u8], index: u32) -> Result<u8> {
        let slot = classes
            .get_mut(index as usize)
            .ok_or_else(string_index_err)?;
        if *slot != CLASS_UNSEEN {
            return Ok(*slot);
        }
        let s = self.get(index)?;
        let key = match s {
            b"highway" => CLASS_HIGHWAY,
            b"name" => CLASS_NAME,
            b"addr:housenumber" => CLASS_HOUSENUMBER,
            b"addr:street" => CLASS_STREET,
            b"addr:place" => CLASS_PLACE,
            b"addr:postcode" => CLASS_POSTCODE,
            b"building" => CLASS_BUILDING,
            b"addr:interpolation" => CLASS_INTERPOLATION,
            _ => CLASS_OTHER,
        };
        let excluded = if crate::tunables::GEOCODE_EXCLUDED_HIGHWAYS.contains(&s) {
            CLASS_EXCLUDED_HIGHWAY
        } else {
            0
        };
        *slot = key | excluded;
        Ok(*slot)
    }
}

/// A tag string index past the block's string table. Refused: the decoded
/// tag iterator would end the way's tags there, silently dropping the rest.
#[cold]
#[inline(never)]
fn string_index_err() -> Error {
    crate::error::new_wire_error("way tag string index lies past the block's string table")
}

/// Per-worker state of the way extraction.
struct ExtractWorker {
    spans: Vec<WayTags>,
    classes: Vec<u8>,
    refs_buf: Vec<i64>,
    segment: Vec<u8>,
    overflow_nodes: Vec<i64>,
    negative_node_ref_count: u64,
    negative_node_id_sample: Vec<i64>,
}

/// What the extraction of one blob reads but never writes.
struct ExtractShared<'a> {
    referenced: &'a crate::idset::IdSet,
    needed_admin_ways: &'a crate::idset::IdSet,
    prealloc_max: i64,
    max_node_id: i64,
}

/// Classify every way of an accepted blob, record the refs of the relevant
/// ones and append their records to `w.segment`.
///
/// Classification is that of the decoded tags (`Way::tags`): the last
/// occurrence of a key decides its value, presence is presence (an empty
/// value is present), and `WayGeocodeFlags::classify` combines them.
fn extract_blob_ways(
    bytes: &[u8],
    strings: &BlobStrings<'_>,
    w: &mut ExtractWorker,
    shared: &ExtractShared<'_>,
) -> Result<()> {
    use super::way_store::{
        ROLE_ADMIN, ROLE_BUILDING_ADDR, ROLE_INTERP, ROLE_STREET, VALUE_HOUSENUMBER,
        VALUE_INTERPOLATION, VALUE_NAME, VALUE_PLACE, VALUE_POSTCODE, VALUE_SLOTS, VALUE_STREET,
    };
    use crate::read::wire::Cursor;

    w.classes.clear();
    w.classes.resize(strings.table.len(), CLASS_UNSEEN);
    w.segment.clear();
    for way in &w.spans {
        let mut keys = Cursor::new(way.keys.in_blob(bytes)?);
        let mut vals = Cursor::new(way.vals.in_blob(bytes)?);
        // Value string index per key, last occurrence winning.
        let mut highway: Option<u32> = None;
        let mut building = false;
        let mut slots: [Option<u32>; VALUE_SLOTS] = [None; VALUE_SLOTS];
        // The validator holds keys and vals to one length.
        while !keys.is_empty() {
            let key = keys.read_varint_u32()?;
            let val = vals.read_varint_u32()?;
            let class = strings.class(&mut w.classes, key)? & CLASS_KEY_MASK;
            // The value is bounds-checked whatever the key, as the decoded
            // tag walk resolves both strings of every tag. Only a highway
            // value is ever classified (below), so the rest are not.
            if val as usize >= strings.table.len() {
                return Err(string_index_err());
            }
            match class {
                CLASS_HIGHWAY => highway = Some(val),
                CLASS_NAME => slots[VALUE_NAME] = Some(val),
                CLASS_HOUSENUMBER => slots[VALUE_HOUSENUMBER] = Some(val),
                CLASS_STREET => slots[VALUE_STREET] = Some(val),
                CLASS_PLACE => slots[VALUE_PLACE] = Some(val),
                CLASS_POSTCODE => slots[VALUE_POSTCODE] = Some(val),
                CLASS_BUILDING => building = true,
                CLASS_INTERPOLATION => slots[VALUE_INTERPOLATION] = Some(val),
                _ => {}
            }
        }
        let excluded = match highway {
            Some(v) => strings.class(&mut w.classes, v)? & CLASS_EXCLUDED_HIGHWAY != 0,
            None => false,
        };
        let flags = WayGeocodeFlags::classify(
            highway.is_some(),
            slots[VALUE_NAME].is_some(),
            excluded,
            building,
            slots[VALUE_HOUSENUMBER].is_some(),
            slots[VALUE_STREET].is_some(),
            slots[VALUE_PLACE].is_some(),
            slots[VALUE_INTERPOLATION].is_some(),
        );
        let mut roles = 0_u8;
        if flags.is_street {
            roles |= ROLE_STREET;
        }
        if flags.is_building_addr {
            roles |= ROLE_BUILDING_ADDR;
        }
        if flags.is_interp {
            roles |= ROLE_INTERP;
        }
        if shared.needed_admin_ways.get(way.id) {
            roles |= ROLE_ADMIN;
        }
        if roles == 0 {
            continue;
        }
        let refs = way.refs.in_blob(bytes)?;
        w.refs_buf.clear();
        w.refs_buf
            .extend(crate::read::elements::WayRefIter::delta_decoded(refs));
        w.negative_node_ref_count = w.negative_node_ref_count.saturating_add(record_way_refs(
            shared.referenced,
            &mut w.overflow_nodes,
            shared.prealloc_max,
            shared.max_node_id,
            &w.refs_buf,
            &mut w.negative_node_id_sample,
        ));
        let mut values: [Option<&[u8]>; VALUE_SLOTS] = [None; VALUE_SLOTS];
        for (value, slot) in values.iter_mut().zip(slots) {
            if let Some(index) = slot {
                *value = Some(strings.get(index)?);
            }
        }
        super::way_store::encode(&mut w.segment, way.id, roles, &values, refs);
    }
    Ok(())
}

/// Pass 1.5: read every way blob once, validated as block construction
/// validates it, and build the geocode-relevant way store
/// (`super::way_store`) while filling the referenced-node set.
#[allow(clippy::too_many_lines)]
#[hotpath::measure]
pub(super) fn run_pass1_5(
    way_schedule: &[ScheduledBlob],
    max_node_id: i64,
    input: &InputFile,
    needed_admin_ways: &crate::idset::IdSet,
    scratch: &crate::scratch::RunScratch,
    host: &crate::host::Entered<'_>,
) -> Result<(crate::idset::IdSet, super::way_store::WayStore)> {
    let shared_file = input.file();
    let file_len = shared_file.metadata()?.len();
    let prealloc_max =
        crate::idset::IdSet::file_bounded_max_id(max_node_id, file_len, NODE_PREALLOC_IDS_PER_BYTE);

    // One shared pre-allocated IdSet; workers write concurrently via
    // `set_atomic`. Replaces the previous per-worker `IdSet`
    // accumulation that grew to ~20 GB anon on Germany (~29.5 GB at planet)
    // because each worker allocated independent chunks across the full
    // planet ID range. Pattern follows `renumber_external/pass1.rs`.
    //
    // `max_node_id` can come from indexdata for indexed blobs. Bound the
    // eager bitmap by input size; workers keep accepted references above
    // that bound in local ID lists, inserted after the scan.
    let mut referenced_nodes = crate::idset::IdSet::new();
    referenced_nodes.pre_allocate(prealloc_max);

    // Custom worker pool that builds no PrimitiveBlock. Workers pread the
    // blob bytes, decompress, and validate them in place with the block
    // validator (so a blob is refused exactly as block construction refuses
    // it, kind claim included), recording each way's spans; then classify
    // the ways from the accepted string table and append the relevant ones
    // to the worker's way-store file. This is the only decode of the way
    // blobs: Pass 2b resolves from the store.
    host.invocation.marker("GEOCODE_PASS1_5_SCAN_START");
    let decode_threads = crate::host::sized_workers(
        &*host.resources,
        crate::tunables::DECODE_WORKERS_RESERVED,
        crate::tunables::DECODE_WORKERS_MIN,
        crate::tunables::DECODE_WORKERS_FALLBACK,
    );
    let mut store_paths = Vec::with_capacity(decode_threads);
    let mut store_files = Vec::with_capacity(decode_threads);
    for worker in 0..decode_threads {
        let path = scratch.file(&format!("geocode-ways-{worker}.bin"))?;
        store_files.push(crate::scratch::create_file(&path)?);
        store_paths.push(path);
    }
    let segments: Mutex<Vec<Option<super::way_store::Segment>>> =
        Mutex::new(vec![None; way_schedule.len()]);
    {
        let shared = ExtractShared {
            referenced: &referenced_nodes,
            needed_admin_ways,
            prealloc_max,
            max_node_id,
        };
        let shared_ref = &shared;
        let segments_ref = &segments;

        // Work-stealing dispatch over way blobs via AtomicUsize::fetch_add:
        // a hand-built claim-counter pool, deliberately not on
        // `crate::ordered_pool` or the classify pools. It has no consumer and
        // no order to keep, and every worker writes the one shared atomic
        // `IdSet` and publishes its overflow list and tallies at its end. The
        // first recorded error (not the lowest blob) stops the others at
        // their next claim, and work in flight finishes; cancellation is
        // checked per claimed blob.
        //
        // No `*_wait_ns` stall counter: nothing here blocks on another
        // thread. The claim is a `fetch_add`, the `IdSet` writes are atomic
        // sets, each worker preads the shared file on its own, and the
        // mutexes are the first-error slot (locked per blob, held for a load)
        // and the end-of-worker hand-offs (once per worker).
        let next_idx = AtomicUsize::new(0);
        let next_ref = &next_idx;
        // First error observed across all workers. Workers drain until
        // the schedule is empty or the shared slot is populated.
        let first_err: Mutex<Option<Error>> = Mutex::new(None);
        let first_err_ref = &first_err;
        let overflow_lists: Mutex<Vec<Vec<i64>>> = Mutex::new(Vec::new());
        let overflow_lists_ref = &overflow_lists;
        let negative_node_ref_count = AtomicU64::new(0);
        let negative_node_ref_count_ref = &negative_node_ref_count;
        let negative_node_ids: Mutex<Vec<i64>> = Mutex::new(Vec::new());
        let negative_node_ids_ref = &negative_node_ids;

        std::thread::scope(|scope| {
            for (worker, store_file) in store_files.into_iter().enumerate() {
                let file = std::sync::Arc::clone(shared_file);
                scope.spawn(move || {
                    let mut read_buf = crate::read::read_buf::ReadBuf::default();
                    let mut decompress_buf: Vec<u8> = Vec::new();
                    let mut st: Vec<(u32, u32)> = Vec::new();
                    let mut groups: Vec<(u32, u32)> = Vec::new();
                    let mut out = std::io::BufWriter::with_capacity(
                        crate::tunables::GEOCODE_DATA_FILE_BUFFER_BYTES,
                        store_file,
                    );
                    let mut out_offset = 0_u64;
                    let mut my_segments: Vec<(usize, super::way_store::Segment)> = Vec::new();
                    let mut w = ExtractWorker {
                        spans: Vec::new(),
                        classes: Vec::new(),
                        refs_buf: Vec::new(),
                        segment: Vec::new(),
                        overflow_nodes: Vec::new(),
                        negative_node_ref_count: 0,
                        negative_node_id_sample: Vec::new(),
                    };
                    let mut overflow_compacted_len = 0_usize;

                    loop {
                        // Exit early if another worker has reported an error.
                        if has_first_err(first_err_ref) {
                            return;
                        }

                        let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                        if idx >= way_schedule.len() {
                            break;
                        }
                        let entry = &way_schedule[idx];

                        let result = (|| -> Result<()> {
                            // Per way blob; a cancellation leaves through the
                            // first-error slot like any blob failure (this
                            // pool has no consumer, and no sequence to keep).
                            host.invocation.check()?;
                            let raw = read_buf.read_exact_at(
                                &*file,
                                entry.data_offset(),
                                entry.data_size(),
                            )?;
                            decompress_buf.clear();
                            crate::blob::decompress_blob_raw(raw, &mut decompress_buf)?;
                            // A blob claiming ways but holding other kinds is
                            // refused by the validator's claim check.
                            w.spans.clear();
                            crate::read::block::validate_in_place(
                                &decompress_buf,
                                &mut st,
                                &mut groups,
                                entry.claim(),
                                |_, _| WayTagSpans { ways: &mut w.spans },
                            )?;
                            let strings = BlobStrings {
                                bytes: &decompress_buf,
                                table: &st,
                            };
                            extract_blob_ways(&decompress_buf, &strings, &mut w, shared_ref)?;
                            if !w.segment.is_empty() {
                                out.write_all(&w.segment)?;
                                let len = w.segment.len() as u64;
                                my_segments.push((
                                    idx,
                                    super::way_store::Segment {
                                        file: worker,
                                        offset: out_offset,
                                        len,
                                    },
                                ));
                                out_offset += len;
                            }
                            compact_overflow_nodes(
                                &mut w.overflow_nodes,
                                &mut overflow_compacted_len,
                            );
                            Ok(())
                        })();
                        read_buf.release_outlier(&host.invocation);
                        crate::blob::release_decode_outlier(&mut decompress_buf, &host.invocation);

                        if let Err(e) = result {
                            store_first_err(first_err_ref, input.locate_blob(entry, e));
                            return;
                        }
                    }

                    // The segments are published only once every byte of the
                    // worker's file is written.
                    if let Err(e) = out.flush() {
                        store_first_err(
                            first_err_ref,
                            Error::from(e).context("failed to write the geocode way store"),
                        );
                        return;
                    }
                    drop(out);
                    {
                        let mut slots = segments_ref
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        for (position, segment) in my_segments {
                            if let Some(slot) = slots.get_mut(position) {
                                *slot = Some(segment);
                            }
                        }
                    }
                    publish_negative_node_refs(
                        negative_node_ref_count_ref,
                        negative_node_ids_ref,
                        w.negative_node_ref_count,
                        w.negative_node_id_sample,
                    );
                    push_overflow_list(overflow_lists_ref, w.overflow_nodes);
                });
            }
        });

        if let Some(e) = first_err
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(e);
        }

        report_negative_node_refs(&negative_node_ref_count, negative_node_ids, host);

        insert_overflow_nodes(
            &mut referenced_nodes,
            overflow_lists
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
    host.invocation.marker("GEOCODE_PASS1_5_SCAN_END");
    let store = super::way_store::WayStore::open(
        store_paths,
        segments
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )?;
    host.invocation
        .counter("geocode_way_store_bytes", store.bytes());
    Ok((referenced_nodes, store))
}

/// Single header pass that produces everything Pass 1.5 and Pass 2a need to
/// start: the way-blob schedule (for Pass 1.5 + unused by Pass 2a), the
/// node-blob schedule (for Pass 2a), the node-ID bound (used for Pass 1.5's
/// size-bounded `IdSet` pre-allocation and verified by Phase 2a), and a
/// single shared file handle reused across both phases' `pread_at` workers.
///
/// Consolidates two previously-separate header walks (Pass 1.5's
/// `build_way_schedule_and_max_node_id` and Pass 2a's
/// `build_classify_schedule(Node)`). At Europe the consolidated walk
/// costs ~16-17 s once, vs 16.6 s + 26.5 s = 43 s for the two-walk
/// shape - measured 2026-04-18 on `bf8f2038`.
///
/// Each schedule is in file order and its `seq` is local to that schedule
/// (node and way are separate `0..n` ranges).
///
/// The node-ID bound is the largest indexdata `max_id` over node blobs,
/// widened by the real maximum of every blob that carries no indexdata.
/// `require_indexdata` at the `build_geocode_index` entry rejects
/// non-indexed input unless `--force` is set; with `--force`, each
/// unindexed blob is decoded once here (in parallel) to learn which kinds
/// it holds and its largest node ID. Indexdata claims still supply the upper
/// bound checked against decoded nodes in Phase 2a; Pass 1.5 separately caps
/// dense `IdSet` pre-allocation by input file size.
#[allow(clippy::type_complexity)]
pub(super) fn build_pass2_schedules(
    input: &PinnedInput,
    host: &crate::host::Entered<'_>,
) -> Result<(
    Vec<ScheduledBlob>, // node_schedule
    Vec<ScheduledBlob>, // way_schedule
    i64,                // max_node_id
    InputFile,
)> {
    let mut walker = crate::read::header_walker::HeaderWalker::open(input, host)?;
    let _ = walker
        .next_header()?
        .ok_or_else(|| crate::error::new_missing_header(input.path()))?;

    // Each schedule is put into file order once both are complete (see
    // `into_file_order` below).
    let mut node_schedule: Vec<ScheduledBlob> = Vec::new();
    let mut way_schedule: Vec<ScheduledBlob> = Vec::new();
    // OsmData blobs without indexdata (a `--force` run). Their kinds and
    // node-ID range are established by decoding them, below.
    let mut unindexed: Vec<ScheduledBlob> = Vec::new();
    let mut max_node_id: i64 = 0;
    while let Some(meta) = walker.next_header()? {
        if !matches!(meta.blob_type, crate::blob::BlobKind::OsmData) {
            continue;
        }
        let entry = ScheduledBlob::from_meta(&meta, crate::scan::classify::KindClaims::Refuse)?;
        let Some(idx) = meta.index.as_ref() else {
            unindexed.push(entry);
            continue;
        };
        // Scheduled on the indexdata kind, believed without a decode: a
        // node in a blob labelled way or relation is never seen by Phase 2a,
        // so its coordinates are missing from the index and the bound check
        // there cannot see it either (reference/indexdata-trust.md).
        match idx.kind {
            crate::ElementKind::Node => {
                if idx.max_id > max_node_id {
                    max_node_id = idx.max_id;
                }
                node_schedule.push(entry);
            }
            crate::ElementKind::Way => way_schedule.push(entry),
            crate::ElementKind::Relation => {}
        }
    }

    let shared_file = walker.input();
    drop(walker);

    // `max_node_id` must bound every node ID Phase 2a will see: Pass 1.5
    // drops refs above it as dangling, and Phase 2a turns a node above it
    // into an error. For a blob with no indexdata there is no claim to read,
    // so decode it and take the real answer - which also says exactly which
    // phases need the blob, instead of feeding every unindexed blob to both.
    // A blob holding both nodes and ways (legal, never indexed by pbfhogg)
    // goes into both schedules; Phase 2a completes before Phase 2b starts,
    // so the in-blob order of the two kinds does not matter.
    //
    // This walk reads only node IDs and kinds, yet builds each block on
    // purpose. It runs only on blobs without indexdata (`--force`), and an
    // estimate says moving it to a projection (`read::project_ids::KindIds`
    // plus the validated block's kinds) would not show: the same move cut
    // check --ids' planet node scan by about 4 s, against a planet geocode
    // build of over 300 s. Not measured on an unindexed planet build.
    if !unindexed.is_empty() {
        let mut found: Vec<Option<UnindexedBlobKinds>> = vec![None; unindexed.len()];
        crate::scan::classify::parallel_classify_phase(
            &shared_file,
            &unindexed,
            None,
            || (),
            |block, _scratch: &mut ()| {
                let mut kinds = UnindexedBlobKinds::default();
                for element in block.elements_skip_metadata() {
                    let node_id = match element {
                        crate::Element::Node(node) => node.id(),
                        crate::Element::DenseNode(node) => node.id(),
                        crate::Element::Way(_) => {
                            kinds.has_ways = true;
                            continue;
                        }
                        crate::Element::Relation(_) => continue,
                    };
                    kinds.has_nodes = true;
                    kinds.max_node_id = kinds.max_node_id.max(node_id);
                }
                Ok(kinds)
            },
            |position, kinds| {
                if let Some(slot) = found.get_mut(position) {
                    *slot = Some(kinds);
                }
            },
            host,
        )?;
        for (entry, kinds) in unindexed.iter().zip(found) {
            let kinds = kinds.ok_or_else(|| {
                Error::internal(format!(
                    "unindexed blob at offset {} was never decoded",
                    entry.frame_start()
                ))
            })?;
            if kinds.has_nodes {
                max_node_id = max_node_id.max(kinds.max_node_id);
                node_schedule.push(*entry);
            }
            if kinds.has_ways {
                way_schedule.push(*entry);
            }
        }
    }
    into_file_order(&mut node_schedule);
    into_file_order(&mut way_schedule);

    host.invocation
        .counter("pass2_node_blobs", node_schedule.len());
    host.invocation
        .counter("pass2_way_blobs", way_schedule.len());
    host.invocation.counter("pass2_max_node_id", max_node_id);

    Ok((node_schedule, way_schedule, max_node_id, shared_file))
}

/// What decoding an unindexed blob says about it: which of the two Pass 2
/// phases need it, and the largest node ID it holds.
#[derive(Clone, Copy, Default)]
struct UnindexedBlobKinds {
    has_nodes: bool,
    has_ways: bool,
    max_node_id: i64,
}

/// Put a schedule into file order; its positions are then its order.
fn into_file_order(schedule: &mut [ScheduledBlob]) {
    schedule.sort_unstable_by_key(ScheduledBlob::frame_start);
}

#[cfg(test)]
mod tests {
    use super::{
        NODE_PREALLOC_IDS_PER_BYTE, OVERFLOW_COMPACT_MIN, compact_overflow_nodes,
        insert_overflow_nodes, record_referenced_node,
    };

    /// Extract one uncompressed block, returning each record as
    /// `(way_id, roles, values, refs)` and the referenced-node set.
    #[allow(clippy::type_complexity)]
    fn extract(
        blob: &[u8],
        admin: &crate::idset::IdSet,
    ) -> (
        Vec<(i64, u8, Vec<Option<String>>, Vec<i64>)>,
        crate::idset::IdSet,
    ) {
        let mut st = Vec::new();
        let mut groups = Vec::new();
        let mut spans = Vec::new();
        crate::read::block::validate_in_place(blob, &mut st, &mut groups, None, |_, _| {
            super::WayTagSpans { ways: &mut spans }
        })
        .expect("valid block");
        let mut referenced = crate::idset::IdSet::new();
        referenced.pre_allocate(1000);
        let mut w = super::ExtractWorker {
            spans,
            classes: Vec::new(),
            refs_buf: Vec::new(),
            segment: Vec::new(),
            overflow_nodes: Vec::new(),
            negative_node_ref_count: 0,
            negative_node_id_sample: Vec::new(),
        };
        let shared = super::ExtractShared {
            referenced: &referenced,
            needed_admin_ways: admin,
            prealloc_max: 1000,
            max_node_id: 1000,
        };
        super::extract_blob_ways(
            blob,
            &super::BlobStrings {
                bytes: blob,
                table: &st,
            },
            &mut w,
            &shared,
        )
        .expect("extract");
        let mut out = Vec::new();
        let mut records = super::super::way_store::SegmentRecords::new(&w.segment);
        while let Some(way) = records.next_way().expect("record") {
            out.push((
                way.way_id,
                way.roles,
                way.values.iter().map(|v| v.map(str::to_owned)).collect(),
                crate::read::elements::WayRefIter::delta_decoded(way.refs).collect(),
            ));
        }
        (out, referenced)
    }

    #[test]
    fn extraction_classifies_as_the_tag_policy_says() {
        use super::super::way_store::{ROLE_ADMIN, ROLE_BUILDING_ADDR, ROLE_INTERP, ROLE_STREET};
        use crate::tunables::GEOCODE_EXCLUDED_HIGHWAYS;
        // All presence combinations, all exclusions, and two admitted values,
        // through real blocks so string-table resolution is part of the check.
        for highway in GEOCODE_EXCLUDED_HIGHWAYS
            .into_iter()
            .chain([b"residential".as_slice(), b"".as_slice()])
        {
            let highway = std::str::from_utf8(highway).expect("ASCII policy");
            for bits in 0u8..128 {
                let fields = [
                    ("highway", highway),
                    ("name", ""),
                    ("building", "no"),
                    ("addr:housenumber", ""),
                    ("addr:street", ""),
                    ("addr:place", ""),
                    ("addr:interpolation", "even"),
                ];
                let tags: Vec<_> = fields
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| bits & (1 << index) != 0)
                    .map(|(_, tag)| tag)
                    .collect();
                let mut builder = crate::block_builder::BlockBuilder::new();
                builder.add_way(1, tags.iter().copied(), &[7, 9], None);
                let blob = builder.take().expect("encode").expect("way");
                let has = |i: u8| bits & (1 << i) != 0;
                let street =
                    has(0) && has(1) && !GEOCODE_EXCLUDED_HIGHWAYS.contains(&highway.as_bytes());
                let building = has(2) && has(3) && (has(4) || has(5));
                let interp = has(6) && !has(3);
                let mut want = 0;
                if street {
                    want |= ROLE_STREET;
                }
                if building {
                    want |= ROLE_BUILDING_ADDR;
                }
                if interp {
                    want |= ROLE_INTERP;
                }
                for admin_way in [false, true] {
                    let mut admin = crate::idset::IdSet::new();
                    if admin_way {
                        admin.set(1);
                    }
                    let want = if admin_way { want | ROLE_ADMIN } else { want };
                    let (records, referenced) = extract(blob, &admin);
                    if want == 0 {
                        assert!(records.is_empty(), "bits {bits:#b} {highway:?}");
                        assert!(!referenced.get(7));
                        continue;
                    }
                    assert_eq!(records.len(), 1, "bits {bits:#b} {highway:?}");
                    let (id, roles, _, refs) = &records[0];
                    assert_eq!((*id, *roles), (1, want), "bits {bits:#b} {highway:?}");
                    assert_eq!(refs, &[7, 9]);
                    assert!(referenced.get(7) && referenced.get(9));
                }
            }
        }
    }

    /// The last occurrence of a key decides its value, and only the values
    /// a role reads are stored; refs outside the collector's range stay in
    /// the record.
    #[test]
    fn extraction_keeps_last_values_and_every_ref() {
        use super::super::way_store::{VALUE_HOUSENUMBER, VALUE_NAME, VALUE_STREET};
        let mut builder = crate::block_builder::BlockBuilder::new();
        builder.add_way(
            5,
            [
                ("highway", "residential"),
                ("name", "Old"),
                ("name", "New"),
                ("addr:housenumber", "3"),
            ],
            &[-4, 2, 5000, 2],
            None,
        );
        let blob = builder.take().expect("encode").expect("way");
        let (records, referenced) = extract(blob, &crate::idset::IdSet::new());
        assert_eq!(records.len(), 1);
        let (_, _, values, refs) = &records[0];
        assert_eq!(values[VALUE_NAME].as_deref(), Some("New"));
        // A street reads no address values.
        assert_eq!(values[VALUE_HOUSENUMBER], None);
        assert_eq!(values[VALUE_STREET], None);
        assert_eq!(refs, &[-4, 2, 5000, 2]);
        assert!(referenced.get(2));
        assert!(!referenced.get(5000), "above the node-ID bound");
    }

    #[test]
    fn indexdata_node_bound_scales_with_input_size() {
        let bound = |claimed, file_len| {
            crate::idset::IdSet::file_bounded_max_id(claimed, file_len, NODE_PREALLOC_IDS_PER_BYTE)
        };
        assert_eq!(bound(i64::MAX, 4096), 4096 * 64);

        let planet_bytes = 80_u64 * 1024 * 1024 * 1024;
        assert_eq!(bound(60_000_000_000, planet_bytes), 60_000_000_000);
        assert_eq!(bound(i64::MAX, u64::MAX), i64::MAX);
    }

    #[test]
    fn references_above_the_dense_bound_survive_overflow_insertion() {
        let mut referenced = crate::idset::IdSet::new();
        referenced.pre_allocate(0);
        let mut overflow = Vec::new();
        let high_id = (1_i64 << 44) + 7;

        record_referenced_node(&referenced, &mut overflow, 0, 0);
        record_referenced_node(&referenced, &mut overflow, 0, high_id);
        record_referenced_node(&referenced, &mut overflow, 0, high_id);
        assert_eq!(overflow, vec![high_id, high_id]);
        insert_overflow_nodes(&mut referenced, vec![overflow]);

        assert!(referenced.get(0));
        assert!(referenced.get(high_id));
        // The pre-allocated chunk plus one chunk for the overflow ID.
        assert_eq!(referenced.allocated_chunk_count(), 2);
    }

    #[test]
    fn overflow_lists_compact_only_after_doubling() {
        let mut compacted = 0;
        let mut small = vec![3, 1, 3];
        compact_overflow_nodes(&mut small, &mut compacted);
        assert_eq!(
            small,
            vec![3, 1, 3],
            "below the minimum, nothing is touched"
        );

        let mut list: Vec<i64> = std::iter::successors(Some(0_i64), |v| Some((v + 1) % 10))
            .take(OVERFLOW_COMPACT_MIN)
            .collect();
        compact_overflow_nodes(&mut list, &mut compacted);
        assert_eq!(list, (0..10).collect::<Vec<i64>>());
        assert_eq!(compacted, 10);
    }
}
