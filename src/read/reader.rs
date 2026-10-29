//! High level reader interface

use super::blob::{Blob, BlobDecode, BlobReader, BlobType, DecompressPool};
use super::block::{HeaderBlock, PrimitiveBlock};
use super::elements::Element;
use super::file_reader::FileReader;
use super::pipeline::PipelineConfig;
use crate::blob_meta::BlobFilter;
use crate::error::{Error, Result, new_missing_header_in};
use crate::tunables::{
    BLOCK_QUEUE, PAR_BATCH_MAX_BLOBS, PAR_BATCH_MAX_BYTES, PAR_INFLIGHT_BUDGET,
    PAR_READER_WORKERS_FALLBACK, PAR_READER_WORKERS_MIN, PAR_READER_WORKERS_RESERVED,
};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;

/// A reader for PBF files that gives access to the stored elements: nodes, ways and relations.
///
/// The PBF header is parsed eagerly at construction time and is accessible via [`header()`](Self::header).
// wontfix(type-generic-bounds): bounds on struct match osmpbf API and document intent
#[derive(Clone, Debug)]
pub struct ElementReader<R: Read + Send> {
    blob_iter: BlobReader<R>,
    resources: Option<crate::host::ResourcesHandle>,
    header: HeaderBlock,
    decode_threads: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
    diagnostics: crate::host::Diagnostics,
}

impl<R: Read + Send> ElementReader<R> {
    /// Creates a new `ElementReader`.
    ///
    /// Reads and parses the PBF header from the first blob. Returns an error if the
    /// first blob is not an `OsmHeader` blob.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let reader = ElementReader::new(buf_reader)?;
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new(reader: R) -> Result<ElementReader<R>> {
        let mut blob_iter = BlobReader::new(reader);
        let header = read_header_blob(&mut blob_iter)?;
        Ok(ElementReader {
            blob_iter,
            header,
            resources: None,
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
            diagnostics: crate::host::Diagnostics::discard(),
        })
    }

    /// Attach per-run diagnostics to this reader.
    #[must_use]
    pub fn diagnostics(mut self, diagnostics: crate::host::Diagnostics) -> Self {
        self.blob_iter = self.blob_iter.diagnostics(diagnostics.clone());
        self.diagnostics = diagnostics;
        self
    }

    /// Run this reader under `invocation` ([`BlobReader::invocation`]):
    /// every iteration method returns `ErrorKind::Cancelled` at its next blob
    /// once the token is tripped, and every counter the iteration records
    /// counts into it. The header blob was read at construction, under the
    /// reader's own token; [`ElementReader::open_with`] binds the invocation
    /// from the open.
    #[must_use]
    pub fn invocation(mut self, invocation: crate::host::Invocation) -> Self {
        self.blob_iter = self.blob_iter.invocation(invocation);
        self
    }

    /// Sets a blob-type filter for the pipelined reader.
    ///
    /// When set, the pipeline skips decompressing blobs whose element type
    /// (from indexdata) does not match the filter. For PBFs without indexdata,
    /// all blobs pass through unchanged.
    ///
    /// This dramatically reduces CPU usage for type-filtered commands: e.g.
    /// filtering for ways only skips decompressing ~85% of blobs (nodes).
    /// This optimization is consumed by the pipelined iteration methods only;
    /// the sequential [`for_each`](Self::for_each) and parallel
    /// [`par_map_reduce`](Self::par_map_reduce) methods process every element.
    pub fn with_blob_filter(mut self, filter: BlobFilter) -> Self {
        self.blob_filter = Some(filter);
        self
    }

    /// Makes the pipelined iteration methods refuse a decoded data block
    /// holding an element of a kind other than its blob's indexdata kind
    /// claim (`ErrorKind::IndexdataMismatch` carrying a `KindContradiction`).
    /// For a run that believes the claim, typically through a kind
    /// [`BlobFilter`]: the check is one compare per decoded block and decodes
    /// nothing it would not. Sequential iteration is unaffected. The
    /// pipelined adoption seam of `crate::scan::classify::KindClaims`.
    #[must_use]
    pub(crate) fn refusing_kind_claims(mut self) -> Self {
        self.pipeline_config
            .set_kind_claims(crate::scan::classify::KindClaims::Refuse);
        self
    }

    /// Sets the number of threads in the decode pool used by
    /// [`for_each_pipelined`](Self::for_each_pipelined),
    /// [`for_each_block_pipelined`](Self::for_each_block_pipelined), and
    /// [`into_blocks_pipelined`](Self::into_blocks_pipelined).
    ///
    /// When not set, the count comes from the reader's host resources (the
    /// machine probe without a host), reserving threads for the I/O reader
    /// and the consumer. The minimum is clamped to 1. The pool is dedicated
    /// to the read, not the host's `Executor`.
    pub fn decode_threads(mut self, n: usize) -> Self {
        self.decode_threads = Some(n.max(1));
        self
    }

    /// Sets Stage 1 pipeline read-ahead depth (raw blobs buffered between I/O and decode).
    ///
    /// Defaults to 16. Values <1 are clamped to 1.
    pub fn read_ahead(mut self, n: usize) -> Self {
        self.pipeline_config.set_read_ahead(n);
        self
    }

    /// Sets Stage 2 pipeline decode-ahead depth.
    ///
    /// Controls decode admission as well as the channel capacity between the
    /// decode pool and the reorder buffer. At most this many decode tasks are
    /// admitted but not yet delivered from the reorder buffer. Lower values
    /// reduce memory usage; higher values absorb decode-time variance.
    ///
    /// Defaults to 32. Values <1 are clamped to 1.
    pub fn decode_ahead(mut self, n: usize) -> Self {
        self.pipeline_config.set_decode_ahead(n);
        self
    }

    /// Returns the PBF file header.
    ///
    /// Contains metadata including bounding box, required/optional features,
    /// writing program, and replication information. Use [`HeaderBlock::is_sorted()`]
    /// to check whether elements are sorted by type then ID.
    pub fn header(&self) -> &HeaderBlock {
        &self.header
    }

    /// Parse each blob header's `pbfhogg.WayMembers-v1` payload (field 5),
    /// so [`crate::blob::Blob::way_members`] answers for the blobs this
    /// reader delivers. Off by default: the payload is authoritative only in
    /// a file whose header declares it, which a caller decides from
    /// [`Self::header`] before switching it on.
    pub(crate) fn parsing_way_members(mut self, enable: bool) -> Self {
        self.blob_iter.set_parse_waymembers(enable);
        self
    }

    /// Decodes the PBF structure sequentially on the calling thread - no background I/O,
    /// no rayon, no channels. Elements are delivered in file order, which is canonical
    /// OSM ID order when [`header().is_sorted()`](HeaderBlock::is_sorted) is `true` and
    /// the file keeps that claim: the reader does not check it, and a caller whose
    /// result depends on the order checks the elements it receives. History files may
    /// repeat an ID for multiple versions.
    ///
    /// This is **6x slower** than [`for_each_pipelined`](Self::for_each_pipelined) on large
    /// files. Prefer `for_each_pipelined` for production workloads - it has the same
    /// `FnMut` signature and file-order guarantee but overlaps I/O with parallel
    /// decompression. Use this method when you need simplicity (no `'static` bound on
    /// the reader) or as a correctness baseline for testing.
    ///
    /// # Errors
    /// Returns the first Error encountered while parsing the PBF structure.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = ElementReader::from_path("tests/test.osm.pbf")?;
    /// let mut ways = 0_u64;
    ///
    /// // Increment the counter by one for each way.
    /// reader.for_each(|element| {
    ///     if let Element::Way(_) = element {
    ///         ways += 1;
    ///     }
    /// })?;
    ///
    /// println!("Number of ways: {ways}");
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    #[hotpath::measure]
    pub fn for_each<F>(self, f: F) -> Result<()>
    where
        F: for<'a> FnMut(Element<'a>),
    {
        // Admitted first, and the consumed reader and callback moved into
        // locals declared after the admission, so the caller's `Read` and
        // closure are destroyed inside it, unwinding included.
        let _call = self.blob_iter.cancel_token().call()?;
        let mut f = f;
        let this = self;
        // `with_blob_filter` configures only the pipelined iteration methods.
        // This sequential API decodes every OsmData blob and does not apply
        // BlobFilter. It is a blob-level omission based on optional indexdata,
        // not a post-decode element predicate shared by every reader engine.
        let Self { mut blob_iter, .. } = this;

        // Loop-local decode scratch. `decompress_into` fills `buf` straight from
        // the compressed payload; `std::mem::take` then MOVES that allocation
        // into the block, leaving `buf` empty so the next iteration allocates a
        // fresh buffer. So `buf` is NOT reused across blobs - its allocation
        // becomes the block's backing store each time (one allocation per block,
        // freed when the block drops). What this route eliminates is the SECOND
        // whole-buffer copy the old `decode()` -> `PrimitiveBlock::new` path paid
        // via `to_vec()` - hundreds of GB of pure memcpy on a planet pass.
        //
        // The genuinely reused buffers are `st_scratch` and `gr_scratch`: the
        // `parse_and_inline_with_scratch` route borrows them per block and hands
        // back their retained capacity, eliminating the per-block
        // `Vec<(u32, u32)>` allocation. `buf` deliberately is not pooled here
        // because the block takes ownership of it.
        let mut buf: Vec<u8> = Vec::new();
        let mut st_scratch: Vec<(u32, u32)> = Vec::new();
        let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
        let source = blob_iter.source().cloned();

        while let Some(blob) = blob_iter.next_blob() {
            let blob = blob?;
            // Only OsmData carries elements. Unknown extension blobs are
            // skipped without decompression, matching the pipeline pump.
            // BlobReader rejects non-leading OSMHeader frames before either
            // path sees them, so later feature claims cannot be discarded.
            if blob.get_type() != BlobType::OsmData {
                continue;
            }
            blob.decompress_into(&mut buf)
                .map_err(|e| blob.locate_error(source.as_ref(), e))?;
            let block = PrimitiveBlock::from_vec_with_scratch(
                std::mem::take(&mut buf),
                &mut st_scratch,
                &mut gr_scratch,
            )
            .map_err(|e| blob.locate_error(source.as_ref(), e))?;
            block.for_each_element(&mut f);
        }

        Ok(())
    }

    /// Decodes the PBF structure using a pipelined approach and calls the given closure on each
    /// element, preserving file order. Overlaps I/O with parallel decompression and protobuf
    /// parsing while delivering elements to an `FnMut` closure on the calling thread.
    ///
    /// Elements are delivered in file order, which is canonical OSM ID order when
    /// [`header().is_sorted()`](HeaderBlock::is_sorted) is `true` and the file keeps that
    /// claim: the reader does not check it. History files may repeat an ID for multiple
    /// versions.
    #[hotpath::measure]
    pub fn for_each_pipelined<F>(self, f: F) -> Result<()>
    where
        F: for<'a> FnMut(Element<'a>),
    {
        // Only elements reach the caller, borrowed from a block that never
        // leaves this method, so no block is issued and none takes a ticket.
        // The reader and callback are destroyed inside the admission.
        let call = self.blob_iter.cancel_token().call()?;
        let mut f = f;
        let this = self;
        super::pipeline::run_pipeline(
            this.blob_iter,
            &call,
            this.decode_threads,
            this.pipeline_config,
            this.blob_filter,
            this.resources,
            this.diagnostics,
            super::pipeline::Blocks,
            |block: PrimitiveBlock| {
                block.for_each_element(&mut f);
                Ok(())
            },
        )
    }

    /// Block-level pipelined iteration. Like [`for_each_pipelined`](Self::for_each_pipelined)
    /// but delivers entire [`PrimitiveBlock`]s (owned) instead of individual elements.
    ///
    /// Blocks arrive in file order. The consumer receives ownership and can send blocks
    /// to other threads for parallel processing, enabling overlapped I/O + decode +
    /// consumer parallelism without blocking the pipeline.
    ///
    /// **Note:** The debug monotonicity assertion for [`Sort.Type_then_ID`](HeaderBlock::is_sorted)
    /// is not applied at this level. Use [`for_each_pipelined`](Self::for_each_pipelined) if you
    /// need it, or check node ID ordering in your consumer closure.
    ///
    /// # Errors
    /// Returns the first error encountered while parsing the PBF structure.
    pub fn for_each_block_pipelined<F>(self, f: F) -> Result<()>
    where
        F: FnMut(PrimitiveBlock) -> Result<()>,
    {
        // The reader and callback are destroyed inside the admission.
        let call = self.blob_iter.cancel_token().call()?;
        let mut f = f;
        let this = self;
        super::pipeline::run_pipeline(
            this.blob_iter,
            &call,
            this.decode_threads,
            this.pipeline_config,
            this.blob_filter,
            this.resources,
            this.diagnostics,
            super::pipeline::Blocks,
            // Each block is issued to the caller's closure, which can keep it.
            |block: PrimitiveBlock| f(block.issued(&call)),
        )
    }

    /// [`for_each_block_pipelined`](Self::for_each_block_pipelined) for the
    /// library's own consumers: the blocks never reach a caller, so none is
    /// issued.
    pub(crate) fn for_each_own_block_pipelined<F>(self, f: F) -> Result<()>
    where
        F: FnMut(PrimitiveBlock) -> Result<()>,
    {
        let call = self.blob_iter.cancel_token().call()?;
        super::pipeline::run_pipeline(
            self.blob_iter,
            &call,
            self.decode_threads,
            self.pipeline_config,
            self.blob_filter,
            self.resources,
            self.diagnostics,
            super::pipeline::Blocks,
            f,
        )
    }

    /// Ordered pipelined read with a per-block transform executed on the
    /// decode workers. The consumer remains serialized on the calling thread.
    ///
    pub(crate) fn for_each_fused_block<T, X, F>(self, transform: X, consume: F) -> Result<()>
    where
        T: Send,
        X: Fn(PrimitiveBlock) -> Result<T> + Sync,
        F: FnMut(T) -> Result<()>,
    {
        self.for_each_fused_blob(super::pipeline::BlockOnly(transform), consume)
    }

    /// [`Self::for_each_fused_block`] with a transform that sees every blob
    /// after the leading header, not only the decoded data blocks: its kind,
    /// indexdata and provenance, for a consumer that checks the envelope in
    /// the same pass that decodes it. See
    /// [`DecodeTransform`](super::pipeline::DecodeTransform).
    pub(crate) fn for_each_fused_blob<X, F>(self, transform: X, consume: F) -> Result<()>
    where
        X: super::pipeline::DecodeTransform,
        F: FnMut(X::Out) -> Result<()>,
    {
        // A nested entry: the command calling it holds its own.
        let call = self.blob_iter.cancel_token().call()?;
        super::pipeline::run_pipeline(
            self.blob_iter,
            &call,
            self.decode_threads,
            self.pipeline_config,
            self.blob_filter,
            self.resources,
            self.diagnostics,
            transform,
            consume,
        )
    }

    /// Returns an iterator of decoded [`PrimitiveBlock`]s from the pipelined reader.
    ///
    /// The 3-stage pipeline (I/O → decode → reorder) runs in a background thread.
    /// Blocks arrive in file order via a bounded channel. The consumer controls
    /// the iteration pace; backpressure propagates naturally when the channel fills.
    /// Dropping the iterator stops the background pipeline promptly, within
    /// about `decode_ahead` blobs, instead of reading the rest of the file.
    ///
    /// This is the iterator equivalent of [`for_each_block_pipelined`](Self::for_each_block_pipelined).
    /// Use it when you need loop control (early exit, zipping two files, interleaving work).
    ///
    /// **Note:** The debug monotonicity assertion for [`Sort.Type_then_ID`](HeaderBlock::is_sorted)
    /// is not applied at this level. Use [`for_each_pipelined`](Self::for_each_pipelined) if you
    /// need it, or check node ID ordering in your consumer code.
    ///
    /// Requires `R: 'static` because the pipeline runs in a background thread.
    /// [`ElementReader<FileReader>`] satisfies this (the common case).
    ///
    /// # Errors
    /// [`ErrorKind::Sealed`](crate::ErrorKind::Sealed) when the reader's
    /// invocation has been sealed; nothing is started.
    pub fn into_blocks_pipelined(self) -> Result<PipelinedBlocks>
    where
        R: 'static,
    {
        let (tx, rx) = sync_channel(BLOCK_QUEUE);
        // Set by the pipeline thread as its last act. A dropped sender with
        // this still false means the thread unwound rather than finished, and
        // the consumer must not read that as end-of-file.
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_completed = std::sync::Arc::clone(&completed);
        let blob_iter = self.blob_iter;
        let decode_threads = self.decode_threads;
        let pipeline_config = self.pipeline_config;
        let blob_filter = self.blob_filter;
        let resources = self.resources;
        let diagnostics = self.diagnostics;
        let call = blob_iter.cancel_token().call()?;
        // The background thread records outside any call, through its own
        // teardown, so it runs under a permit it holds to its last act. The
        // session itself only receives and joins, and records nothing.
        let permit = call.permit();
        let invocation = call.handle().clone();
        let ticket = call.ticket(crate::host::Kind::Session);

        let handle = std::thread::spawn(move || {
            let permit = permit;
            let deliver = |block: PrimitiveBlock| {
                tx.send(Ok(block))
                    .map_err(|_| Error::internal("pipeline consumer dropped"))
            };
            let result = super::pipeline::run_pipeline(
                blob_iter,
                &permit,
                decode_threads,
                pipeline_config,
                blob_filter,
                resources,
                diagnostics,
                super::pipeline::Blocks,
                deliver,
            );
            if let Err(e) = result {
                // Deliver the error as the last iterator item.
                // Ignore send failure - consumer may have already dropped.
                drop(tx.send(Err(e)));
            }
            thread_completed.store(true, std::sync::atomic::Ordering::Release);
            drop(permit);
        });

        Ok(PipelinedBlocks {
            rx: Some(rx),
            handle: Some(handle),
            completed,
            invocation,
            _ticket: ticket,
        })
    }

    /// Parallel map/reduce. Decodes the PBF structure in parallel, calls the closure `map_op` on
    /// each element and then reduces the number of results to one item with the closure
    /// `reduce_op`. Similarly to the `init` argument in the `fold` method on iterators, the
    /// `identity` closure should produce an identity value that is inserted into `reduce_op` when
    /// necessary. The number of times that this identity value is inserted should not alter the
    /// result.
    ///
    /// **Note:** Elements are delivered in arbitrary order across worker threads.
    /// The [`Sort.Type_then_ID`](HeaderBlock::is_sorted) ordering guarantee does **not**
    /// apply to this method. Use [`for_each`](Self::for_each) or
    /// [`for_each_pipelined`](Self::for_each_pipelined) if you need sorted element order.
    ///
    /// # Memory
    ///
    /// One-pass and byte-bounded: the calling thread pumps compressed blobs
    /// from the file into count- and byte-bounded batches, admitting each blob
    /// against a fixed 256 MiB in-flight byte budget before reading further.
    /// Long-lived decode/map workers each fold into a
    /// single worker-local `T`; the caller then reduces at most one partial per
    /// worker. Total reader working set stays well under 2 GiB regardless of
    /// file size - even with pathological 32 MiB blobs - because nothing
    /// accumulates the whole file. This is the key difference from a
    /// collect-everything-then-decode design, which pinned ~1x the file size in
    /// compressed blobs and OOM-killed at planet scale.
    ///
    /// # Errors
    /// Returns the first Error encountered while parsing the PBF structure.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = ElementReader::from_path("tests/test.osm.pbf")?;
    ///
    /// // Count the ways
    /// let ways = reader.par_map_reduce(
    ///     |element| {
    ///         match element {
    ///             Element::Way(_) => 1,
    ///             _ => 0,
    ///         }
    ///     },
    ///     || 0_u64,      // Zero is the identity value for addition
    ///     |a, b| a + b   // Sum the partial results
    /// )?;
    ///
    /// println!("Number of ways: {ways}");
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn par_map_reduce<MP, RD, ID, T>(self, map_op: MP, identity: ID, reduce_op: RD) -> Result<T>
    where
        MP: for<'a> Fn(Element<'a>) -> T + Sync + Send,
        RD: Fn(T, T) -> T + Sync + Send,
        ID: Fn() -> T + Sync + Send,
        T: Send,
    {
        // The reader and closures are destroyed inside the admission,
        // unwinding included.
        let _call = self.blob_iter.cancel_token().call()?;
        let closures = (map_op, identity, reduce_op);
        let mut this = self;
        // `with_blob_filter` configures only the pipelined iteration methods.
        // par_map_reduce maps every decoded element and intentionally disables
        // indexdata parsing; using the optional metadata filter here would make
        // results depend on indexdata presence and correctness.
        // par_map_reduce never inspects blob.index(); skip the per-blob
        // indexdata copy in the frame pump.
        this.blob_iter.set_parse_indexdata(false);

        // Reserve one thread for the frame pump (this thread); the rest decode.
        let worker_count = this.decode_threads.unwrap_or_else(|| {
            crate::host::sized_workers(
                this.resources.as_ref().map_or(
                    &crate::host::MachineProbe as &dyn crate::host::Resources,
                    |r| &*r.0,
                ),
                PAR_READER_WORKERS_RESERVED,
                PAR_READER_WORKERS_MIN,
                PAR_READER_WORKERS_FALLBACK,
            )
        });

        par_fold_blobs(
            this.blob_iter,
            worker_count,
            PAR_INFLIGHT_BUDGET,
            PAR_BATCH_MAX_BLOBS,
            PAR_BATCH_MAX_BYTES,
            closures.0,
            closures.1,
            closures.2,
        )
    }
}

/// Worker-resident parallel fold shared by [`ElementReader::par_map_reduce`] and
/// its failure-path tests. The frame pump runs on the calling thread under a
/// fixed in-flight byte budget; scoped workers fold each into one partial `T`;
/// the caller reduces at most one partial per worker on complete success.
///
/// Cancellation and panic safety are handled by RAII guards installed before any
/// worker spawns, so no unwind - from a panicking `map_op`/`reduce_op`/`identity`,
/// a panicking `Read`, or a spawn failure - can leave the pump or a sibling
/// worker blocked forever:
///
/// - [`CancelGuard`] on the calling thread closes the queue and shuts the budget
///   down if the calling thread unwinds inside the scope before the pump returns
///   normally (finding: spawn/`Read` panic deadlocks scoped cleanup).
/// - a per-worker [`CancelGuard`] does the same if a worker unwinds, so a lone
///   `decode_threads(1)` worker cannot strand the pump in `ByteBudget::acquire`.
/// - [`BatchCharge`] releases a worker's held budget on every batch exit -
///   normal, decode error, or mid-fold panic - after freeing the compressed
///   storage, so budget capacity never leaks and never frees before the bytes
///   it accounts for actually drop.
///
/// The budget must admit the pump's next blob even when only its own
/// undispatched batch still holds capacity (see [`pump_blobs`]); the
/// production constants are checked at compile time, and any other
/// combination by a debug assertion here.
#[allow(clippy::too_many_arguments)]
fn par_fold_blobs<R, MP, RD, ID, T>(
    blob_iter: BlobReader<R>,
    worker_count: usize,
    budget_cap: u64,
    batch_max_blobs: usize,
    batch_max_bytes: u64,
    map_op: MP,
    identity: ID,
    reduce_op: RD,
) -> Result<T>
where
    R: Read + Send,
    MP: for<'a> Fn(Element<'a>) -> T + Sync + Send,
    RD: Fn(T, T) -> T + Sync + Send,
    ID: Fn() -> T + Sync + Send,
    T: Send,
{
    debug_assert!(
        pump_cannot_wait_on_own_batch(budget_cap, batch_max_blobs, batch_max_bytes),
        "par_fold_blobs budget {budget_cap} cannot hold a {batch_max_bytes}-byte \
         partial batch plus one more blob"
    );
    let queue = Arc::new(BatchQueue::new());
    let budget = Arc::new(ByteBudget::new(budget_cap));
    // Nested under the caller's admission, so never refused there.
    let call = blob_iter.cancel_token().call()?;
    let pool = DecompressPool::new(&call);
    drop(call);
    // Captured once; every worker borrows it to locate decode failures.
    let source = blob_iter.source().cloned();
    let source = source.as_ref();

    // Borrow the caller closures by shared reference so every worker shares
    // one instance. Their `Sync + Send` bounds make the shared references
    // safe to move into the scoped worker threads.
    let map_op = &map_op;
    let identity = &identity;
    let reduce_op = &reduce_op;

    std::thread::scope(|scope| -> Result<T> {
        // Installed before the first worker spawns: if the calling thread
        // unwinds inside this scope - a `scope.spawn` failure, or a panicking
        // `R::read` inside the pump - this closes the queue and shuts the budget
        // down so every already-spawned worker wakes and scope-join can finish
        // the unwind instead of blocking forever on a queue that would never be
        // closed. Disarmed once the pump returns normally and teardown moves to
        // the join loop below.
        let mut cancel = CancelGuard::new(&queue, &budget);

        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let queue = Arc::clone(&queue);
            let budget = Arc::clone(&budget);
            let pool = Arc::clone(&pool);
            handles.push(scope.spawn(move || -> Result<T> {
                run_par_worker(&queue, &budget, &pool, source, map_op, identity, reduce_op)
            }));
        }

        // Frame pump runs on the calling thread: sequential file I/O with
        // kernel readahead preserved, feeding bounded batches to workers.
        let pump_result = pump_blobs(blob_iter, &queue, &budget, batch_max_blobs, batch_max_bytes);
        // On a read error, wake and stop workers promptly; on success let them
        // drain the queue to completion (never shut the budget down, which would
        // make workers skip queued batches).
        if pump_result.is_err() {
            budget.shutdown();
        }
        queue.close();
        // The pump returned normally (Ok or Err, not a panic); teardown is now
        // the join loop's job, so defuse the calling-thread guard.
        cancel.disarm();

        // Join every worker, collecting at most one partial each. Do NOT reduce
        // yet: a panic in `identity`/`reduce_op` must not mask an already-known
        // parse error, and read/framing errors must win over decode errors.
        // Worker panics propagate deterministically here (mirroring rayon).
        let mut partials: Vec<T> = Vec::with_capacity(worker_count);
        let mut worker_err: Option<crate::error::Error> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(partial)) => partials.push(partial),
                Ok(Err(e)) => {
                    if worker_err.is_none() {
                        worker_err = Some(e);
                    }
                }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }

        // Read/framing errors take precedence, matching the prior collect-first
        // behavior where a bad blob aborted before any decode ran. Then worker
        // decode errors. Both are decided before any user-closure reduction, so
        // a panic in the final fold cannot replace a real parsing error.
        pump_result?;
        if let Some(e) = worker_err {
            return Err(e);
        }

        // Complete success: only now fold the per-worker partials.
        let mut acc = identity();
        for partial in partials {
            acc = reduce_op(acc, partial);
        }
        Ok(acc)
    })
}

/// A count- and byte-bounded run of compressed OsmData blobs handed to a worker.
struct Batch {
    blobs: Vec<Blob>,
    /// Sum of the retained byte weights admitted for `blobs`. Released back to
    /// the [`ByteBudget`] as a unit once the worker finishes the batch.
    bytes: u64,
}

impl Batch {
    fn new() -> Self {
        Self {
            blobs: Vec::new(),
            bytes: 0,
        }
    }
}

/// RAII release of a worker's held batch budget.
///
/// Owns the batch for the duration of decode/fold so that on *any* exit -
/// normal end of batch, decode error, or a panic in `map_op`/`reduce_op` mid
/// fold - the compressed blob storage is freed *before* the accounted bytes are
/// returned to the [`ByteBudget`]. Releasing before the storage drops would let
/// the pump admit replacement bytes while the old allocations are still
/// resident, transiently exceeding the in-flight cap.
struct BatchCharge<'a> {
    budget: &'a ByteBudget,
    batch: Batch,
}

impl<'a> BatchCharge<'a> {
    fn new(budget: &'a ByteBudget, batch: Batch) -> Self {
        Self { budget, batch }
    }

    fn blobs(&self) -> &[Blob] {
        &self.batch.blobs
    }
}

impl Drop for BatchCharge<'_> {
    fn drop(&mut self) {
        let bytes = self.batch.bytes;
        // Free the compressed blob storage FIRST, then return capacity, so the
        // pump can never admit replacement bytes while these allocations are
        // still resident.
        self.batch.blobs = Vec::new();
        self.budget.release(bytes);
    }
}

/// Cancellation guard for the calling thread and each worker. On an armed drop -
/// i.e. an unwind that never reached a matching [`disarm`](Self::disarm) - it
/// closes the queue and shuts the budget down, waking the pump and every blocked
/// worker so scoped-thread join can complete instead of deadlocking on a queue
/// that would otherwise never be closed. A clean exit disarms it first.
struct CancelGuard<'a> {
    queue: &'a BatchQueue,
    budget: &'a ByteBudget,
    armed: bool,
}

impl<'a> CancelGuard<'a> {
    fn new(queue: &'a BatchQueue, budget: &'a ByteBudget) -> Self {
        Self {
            queue,
            budget,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.budget.shutdown();
            self.queue.close();
        }
    }
}

/// Sequential frame pump: reads compressed blobs on the calling thread,
/// preserving single-stream file I/O and kernel readahead, and feeds bounded
/// batches to the worker queue. Acquires in-flight byte capacity before
/// admitting each blob so the queued working set stays bounded. Stops promptly
/// if a worker signals shutdown; propagates the first read error.
///
/// A read/framing error is inspected *before* the shutdown check, so a read
/// error the pump has already obtained is never discarded because a worker
/// happened to signal shutdown in the same window - read errors win over decode
/// errors deterministically.
/// Whether the pump can never wait in `ByteBudget::acquire` for capacity its
/// own undispatched batch holds.
///
/// The pump acquires a blob's weight before deciding whether to flush the
/// batch it is building, so that batch's bytes are admitted but no worker can
/// release them. Once every dispatched batch has released, the budget holds
/// only the partial batch (at most `batch_max_bytes`, or one lone blob of up
/// to `MAX_BLOB_DATASIZE` that exceeds it), and the next blob (at most
/// `MAX_BLOB_DATASIZE`) must fit beside it or the pump waits forever. A
/// one-blob batch is flushed as soon as it forms, so it never holds anything
/// at an acquire.
const fn pump_cannot_wait_on_own_batch(
    budget_cap: u64,
    batch_max_blobs: usize,
    batch_max_bytes: u64,
) -> bool {
    use crate::read::blob_wire::MAX_BLOB_DATASIZE;
    let held = if batch_max_bytes > MAX_BLOB_DATASIZE {
        batch_max_bytes
    } else {
        MAX_BLOB_DATASIZE
    };
    batch_max_blobs <= 1 || budget_cap >= held + MAX_BLOB_DATASIZE
}

const _: () = assert!(
    pump_cannot_wait_on_own_batch(
        PAR_INFLIGHT_BUDGET,
        PAR_BATCH_MAX_BLOBS,
        PAR_BATCH_MAX_BYTES
    ),
    "PAR_INFLIGHT_BUDGET must hold a full partial batch plus one more blob"
);

fn pump_blobs<R: Read + Send>(
    mut blob_iter: BlobReader<R>,
    queue: &BatchQueue,
    budget: &ByteBudget,
    batch_max_blobs: usize,
    batch_max_bytes: u64,
) -> Result<()> {
    let mut batch = Batch::new();
    // Under the method's call; no admission per blob.
    while let Some(blob_result) = blob_iter.next_blob() {
        // Resolve the read result before consulting shutdown: an obtained
        // read/framing error must propagate and win over any worker decode
        // error, even if a worker signaled shutdown while this blob was read.
        let blob = blob_result?;
        if budget.is_shutdown() {
            break;
        }
        if blob.get_type() != BlobType::OsmData {
            continue; // non-OsmData blobs carry no elements
        }
        let weight = blob.retained_len();
        if !budget.acquire(weight) {
            break; // a worker errored while we waited for capacity
        }
        // Byte cap is a hard cap: flush the current batch before a blob would
        // carry it past the target. A lone blob heavier than the whole target
        // still forms its own one-element batch (never split).
        if !batch.blobs.is_empty() && batch.bytes.saturating_add(weight) > batch_max_bytes {
            queue.push(std::mem::replace(&mut batch, Batch::new()));
        }
        batch.bytes += weight;
        batch.blobs.push(blob);
        // Count cap is exact: appended then flushed at the limit, never past it.
        if batch.blobs.len() >= batch_max_blobs {
            queue.push(std::mem::replace(&mut batch, Batch::new()));
        }
    }
    if !batch.blobs.is_empty() {
        queue.push(batch);
    }
    Ok(())
}

/// Long-lived decode/map worker: pulls bounded batches, reuses worker-local
/// decompression scratch, folds every element into one worker-local `T`, and
/// returns byte capacity as each batch completes. Returns the single partial
/// (or the first decode error, after signaling shutdown so the pump stops).
///
/// A [`CancelGuard`] armed for the whole worker turns any unwind - through
/// `identity`, `map_op`, or `reduce_op` - into a queue close plus budget
/// shutdown, so a panicking worker (even the only one) cannot strand the pump
/// blocked in `ByteBudget::acquire`. Held batch budget is released by
/// [`BatchCharge`] regardless of how the batch exits.
fn run_par_worker<MP, RD, ID, T>(
    queue: &BatchQueue,
    budget: &ByteBudget,
    pool: &Arc<DecompressPool>,
    source: Option<&Arc<std::path::Path>>,
    map_op: &MP,
    identity: &ID,
    reduce_op: &RD,
) -> Result<T>
where
    MP: for<'a> Fn(Element<'a>) -> T,
    RD: Fn(T, T) -> T,
    ID: Fn() -> T,
{
    // Armed before `identity()` so even a panic there cancels cleanly.
    let mut cancel = CancelGuard::new(queue, budget);
    let mut st_scratch: Vec<(u32, u32)> = Vec::new();
    let mut gr_scratch: Vec<(u32, u32)> = Vec::new();
    let mut acc = identity();
    while let Some(batch) = queue.pop() {
        if budget.is_shutdown() {
            // Free storage then release; shutdown is already signaled.
            drop(BatchCharge::new(budget, batch));
            break;
        }
        // BatchCharge owns the batch: on normal end, decode error, or a mid-fold
        // panic it frees the compressed storage before returning capacity.
        let charge = BatchCharge::new(budget, batch);
        let mut decode_err: Option<crate::error::Error> = None;
        for blob in charge.blobs() {
            match blob.to_primitiveblock_inline_with_scratch(pool, &mut st_scratch, &mut gr_scratch)
            {
                Ok(block) => {
                    for element in block.elements() {
                        acc = reduce_op(acc, map_op(element));
                    }
                }
                Err(e) => {
                    decode_err = Some(blob.locate_error(source, e));
                    break;
                }
            }
        }
        if let Some(e) = decode_err {
            // Signal shutdown/close BEFORE dropping the charge (which releases
            // this batch's bytes) so a blocked pump cannot slip one extra blob
            // through the freed capacity on the way out.
            budget.shutdown();
            queue.close();
            cancel.disarm(); // cancellation performed explicitly above
            drop(charge); // frees storage, then releases budget
            return Err(e);
        }
        // Normal end of batch: charge drops here (frees storage, then releases).
    }
    // Clean drain: leave siblings to finish; do not shut the budget down.
    cancel.disarm();
    Ok(acc)
}

/// Closable multi-consumer queue of [`Batch`]es between the single frame pump
/// and the decode workers. Memory is bounded by the [`ByteBudget`] the pump
/// acquires before pushing, so the queue itself is unbounded in slot count.
struct BatchQueue {
    inner: Mutex<BatchQueueState>,
    cond: Condvar,
}

struct BatchQueueState {
    batches: VecDeque<Batch>,
    closed: bool,
}

impl BatchQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(BatchQueueState {
                batches: VecDeque::new(),
                closed: false,
            }),
            cond: Condvar::new(),
        }
    }

    fn push(&self, batch: Batch) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.batches.push_back(batch);
        drop(state);
        self.cond.notify_one();
    }

    /// Marks the queue closed and wakes every blocked consumer. After close,
    /// `pop` drains remaining batches then returns `None`.
    fn close(&self) {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.cond.notify_all();
    }

    /// Blocks until a batch is available or the queue is closed and drained.
    /// The condvar wait releases the lock, so a blocked consumer never stalls
    /// its peers.
    fn pop(&self) -> Option<Batch> {
        let mut state = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(batch) = state.batches.pop_front() {
                return Some(batch);
            }
            if state.closed {
                return None;
            }
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Fixed budget for compressed bytes in flight (queued plus under decode).
///
/// The frame pump is the sole acquirer; workers release as batches finish. A
/// blob is admitted whenever it fits under the cap, or unconditionally when the
/// budget is empty so a single legal blob larger than the cap cannot deadlock.
/// This is the only case where `used` may exceed `cap`; while that blob is
/// charged, every later acquire waits for its release.
/// `shutdown` wakes a blocked pump so worker errors cancel the read promptly.
struct ByteBudget {
    state: Mutex<ByteBudgetState>,
    cond: Condvar,
    cap: u64,
}

struct ByteBudgetState {
    used: u64,
    shutdown: bool,
}

impl ByteBudget {
    fn new(cap: u64) -> Self {
        Self {
            state: Mutex::new(ByteBudgetState {
                used: 0,
                shutdown: false,
            }),
            cond: Condvar::new(),
            cap: cap.max(1),
        }
    }

    /// Acquires `n` bytes of capacity, blocking until it fits. Returns `false`
    /// if shutdown was signaled instead of admitting the bytes.
    fn acquire(&self, n: u64) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if state.shutdown {
                return false;
            }
            if state.used == 0 || state.used + n <= self.cap {
                state.used += n;
                return true;
            }
            state = self
                .cond
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn release(&self, n: u64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        debug_assert!(
            state.used >= n,
            "ByteBudget released {n} bytes with only {} admitted",
            state.used
        );
        state.used = state.used.saturating_sub(n);
        drop(state);
        self.cond.notify_one();
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.shutdown = true;
        drop(state);
        self.cond.notify_all();
    }

    fn is_shutdown(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .shutdown
    }
}

impl ElementReader<FileReader> {
    /// The metadata of the file this reader opened (an `fstat` of its own
    /// descriptor, never a stat of the path).
    pub(crate) fn file_metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.blob_iter.file_metadata()
    }

    /// Tries to open the file at the given path and constructs an `ElementReader` from this.
    ///
    /// Reads and parses the PBF header from the first blob. Returns an error if the file
    /// cannot be opened or the first blob is not an `OsmHeader` blob.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = ElementReader::from_path("tests/test.osm.pbf")?;
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut blob_iter = BlobReader::from_path(path)?;
        let header = read_header_blob(&mut blob_iter)?;
        Ok(ElementReader {
            blob_iter,
            header,
            resources: None,
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
            diagnostics: crate::host::Diagnostics::discard(),
        })
    }

    /// Open a file for reading with O_DIRECT (bypasses page cache).
    #[cfg(feature = "linux-direct-io")]
    pub fn from_path_direct<P: AsRef<Path>>(path: P) -> Result<Self> {
        let mut blob_iter = BlobReader::from_path_direct(path)?;
        let header = read_header_blob(&mut blob_iter)?;
        Ok(ElementReader {
            blob_iter,
            header,
            resources: None,
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
            diagnostics: crate::host::Diagnostics::discard(),
        })
    }

    /// Open a file, choosing page-cache eviction up front.
    ///
    /// Forwards to [`BlobReader::evict_page_cache`]; see that method for what
    /// eviction does and when you might want it. Eviction is off by default
    /// for the file constructors.
    ///
    /// This is the constructor to use when eviction should cover the *whole*
    /// file. [`ElementReader::from_path`] reads and decodes the header blob
    /// before it returns, and the eviction decision has to be made before that
    /// read - see [`ElementReader::evict_page_cache`].
    ///
    /// ```
    /// # fn foo() -> pbfhogg::Result<()> {
    /// // A one-pass read of a file larger than RAM, header included.
    /// let reader = pbfhogg::ElementReader::from_path_with_eviction("tests/test.osm.pbf", true)?;
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn from_path_with_eviction<P: AsRef<Path>>(path: P, enabled: bool) -> Result<Self> {
        let mut blob_iter = BlobReader::from_path(path)?.evict_page_cache(enabled);
        let header = read_header_blob(&mut blob_iter)?;
        Ok(ElementReader {
            blob_iter,
            header,
            resources: None,
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
            diagnostics: crate::host::Diagnostics::discard(),
        })
    }

    /// Enable or disable page-cache eviction behind the read head, from the
    /// *next* blob onwards.
    ///
    /// Forwards to [`BlobReader::evict_page_cache`]. Note the "from the next
    /// blob onwards": every `ElementReader` file constructor reads and decodes
    /// the header blob before returning, so by the time this method can be
    /// called the header has already been read under the constructor's choice
    /// (no eviction, for the public constructors). Passing `true` here
    /// therefore evicts behind the element blobs, and the header's pages go
    /// with the first of them, because the advised range is the whole consumed
    /// prefix. Use [`ElementReader::from_path_with_eviction`] to fix the choice
    /// before the header is read.
    #[must_use]
    pub fn evict_page_cache(mut self, enabled: bool) -> Self {
        self.blob_iter = self.blob_iter.evict_page_cache(enabled);
        self
    }

    /// Open a file under `host`: buffered or O_DIRECT per `direct`, with the
    /// host's diagnostics, resources, page-cache policy and invocation, all
    /// applied before the header blob is read. Like the other public
    /// constructors it reads any file, partially indexed or not.
    pub fn open_with<'a>(
        source: impl Into<crate::read::input::InputSource<'a>>,
        direct: bool,
        host: &crate::host::Host,
    ) -> Result<Self> {
        let host = &host.enter()?;
        Self::with_blobs(BlobReader::open_with(source, direct, host)?, host)
    }

    /// Open a file for a command run under `host` ([`Self::open_with`]). As a
    /// command input it refuses a partially indexed file in every iteration
    /// mode, whatever blob filter is set ([`BlobReader::open_in`]).
    pub(crate) fn open_in<'a>(
        source: impl Into<crate::read::input::InputSource<'a>>,
        direct: bool,
        host: &crate::host::Entered<'_>,
    ) -> Result<Self> {
        Self::with_blobs(BlobReader::open_in(source, direct, host)?, host)
    }

    /// An element reader over `blob_iter`, opened under `host`: reads the
    /// header blob.
    fn with_blobs(
        mut blob_iter: BlobReader<FileReader>,
        host: &crate::host::Entered<'_>,
    ) -> Result<Self> {
        let header = read_header_blob(&mut blob_iter)?;
        Ok(Self {
            blob_iter,
            header,
            resources: Some(crate::host::ResourcesHandle(std::sync::Arc::clone(
                &host.resources,
            ))),
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
            diagnostics: host.diagnostics.clone(),
        })
    }

    /// Open a file, selecting buffered or O_DIRECT based on the `direct` flag.
    /// The page cache is left alone; see [`ElementReader::evict_page_cache`].
    pub fn open<P: AsRef<Path>>(path: P, direct: bool) -> Result<Self> {
        let mut blob_iter = BlobReader::open(path, direct)?;
        let header = read_header_blob(&mut blob_iter)?;
        Ok(ElementReader {
            blob_iter,
            header,
            resources: None,
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
            diagnostics: crate::host::Diagnostics::discard(),
        })
    }
}

/// Read and parse the header blob from a `BlobReader`.
///
/// Consumes the first blob from the reader. Returns an error if there are no blobs
/// or the first blob is not an `OsmHeader`.
fn read_header_blob<R: Read + Send>(blob_iter: &mut BlobReader<R>) -> Result<HeaderBlock> {
    match blob_iter.next() {
        Some(Ok(blob)) => match blob
            .decode()
            .map_err(|e| blob.locate_error(blob_iter.source(), e))?
        {
            BlobDecode::OsmHeader(header) => Ok(*header),
            _ => Err(new_missing_header_in(blob_iter.source())),
        },
        Some(Err(e)) => Err(e),
        None => Err(new_missing_header_in(blob_iter.source())),
    }
}

// ---------------------------------------------------------------------------
// PipelinedBlocks iterator
// ---------------------------------------------------------------------------

/// Iterator over decoded [`PrimitiveBlock`]s from a pipelined PBF reader.
///
/// Created by [`ElementReader::into_blocks_pipelined`]. The 3-stage pipeline
/// runs in a background thread; blocks are delivered in file order via a bounded
/// channel. Dropping this iterator signals the pipeline to shut down promptly,
/// and waits for it: a pipeline blocked in a `Read` the caller supplied is
/// waited for until that read returns, which is the caller's to ensure.
pub struct PipelinedBlocks {
    rx: Option<Receiver<Result<PrimitiveBlock>>>,
    handle: Option<JoinHandle<()>>,
    /// Set by the pipeline thread immediately before it returns. Distinguishes
    /// "the sender was dropped because the work finished" from "the sender was
    /// dropped because the thread unwound".
    completed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The invocation the pipeline runs under: the handle that admits each
    /// `next` and issues each block it returns.
    invocation: crate::host::Invocation,
    /// This session's census ticket. It retires with the session; the
    /// background thread holds the pipeline's permit itself.
    _ticket: crate::host::Ticket,
}

impl Iterator for PipelinedBlocks {
    type Item = Result<PrimitiveBlock>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rx.as_ref()?;
        // Refused only once the invocation sealed, which its pipeline's
        // permit rules out until the pipeline has finished: what it left
        // queued is then given up, once, and the iterator ends.
        let call = match self.invocation.call() {
            Ok(call) => call,
            Err(sealed) => {
                self.rx = None;
                return Some(Err(sealed));
            }
        };
        match self.rx.as_ref()?.recv() {
            Ok(item) => Some(item.map(|block| block.issued(&call))),
            Err(_) => {
                // The channel closed. On the normal path the pipeline thread
                // has already sent every block (and any error) and set
                // `completed` before dropping its sender, so this is
                // end-of-iteration. On a panic it dropped the sender while
                // unwinding with `completed` still false - and `Drop` below
                // cannot return an error, so returning `None` here
                // would report a crashed pipeline as a cleanly ended file.
                self.rx = None;
                if self.completed.load(std::sync::atomic::Ordering::Acquire) {
                    None
                } else {
                    let message = match self.handle.take().map(JoinHandle::join) {
                        Some(Err(payload)) => format!(
                            "pipeline thread terminated without completing (panicked): {}",
                            crate::error::panic_message(payload.as_ref()),
                        ),
                        _ => "pipeline thread terminated without completing".to_owned(),
                    };
                    Some(Err(Error::internal(message)))
                }
            }
        }
    }
}

impl Drop for PipelinedBlocks {
    fn drop(&mut self) {
        // Close the channel first - signals the pipeline to shut down.
        drop(self.rx.take());
        // Join the background thread (waits for pipeline cleanup). An early
        // iterator drop has no error return channel. Do not resume its panic:
        // this may already be cleanup during another unwind. Exhausting the
        // iterator reports the payload through `next` instead.
        if let Some(h) = self.handle.take() {
            drop(h.join());
        }
    }
}

// Tests use `unwrap()` freely: a panic is the correct failure mode for a unit
// test - it fails immediately with a backtrace at the exact call site. The
// crate-wide `unwrap_used = "deny"` lint targets production code.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Batch, BatchQueue, ByteBudget, ElementReader, PipelinedBlocks, par_fold_blobs};
    use crate::error::{BlobError, ErrorKind};
    use std::io::{Cursor, Read};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Public file readers keep the page cache unless asked, and an eviction
    /// request has to be supplied before the header blob is read to cover it.
    ///
    /// The distinction this test draws is "the header was read without
    /// eviction" versus "the header was evicted". A reader in either state can
    /// carry an armed `evict_prefix` callback afterwards, so the callback
    /// cannot tell them apart;
    /// the fadvise call count can, and it is what the kernel actually saw.
    #[test]
    #[cfg(target_os = "linux")]
    fn public_readers_keep_the_page_cache_unless_asked() {
        let path = "tests/test.osm.pbf";

        // The default keeps: the header blob is read and nothing is advised.
        let default_keep = ElementReader::from_path(path).unwrap();
        assert_eq!(
            default_keep.blob_iter.evict_call_count(),
            0,
            "a public reader must not evict by default"
        );

        // Post-hoc opt-in: the header blob was already read under the default,
        // so no fadvise has been issued for it yet.
        let late = ElementReader::from_path(path)
            .unwrap()
            .evict_page_cache(true);
        assert_eq!(
            late.blob_iter.evict_call_count(),
            0,
            "from_path reads the header eagerly, so a post-hoc opt-in cannot \
             have advised the header blob - if this is 1 the constructor stopped \
             honouring the default and the doc comment on evict_page_cache is stale"
        );

        // Up-front opt-in: the header blob itself is advised away.
        let early = ElementReader::from_path_with_eviction(path, true).unwrap();
        assert_eq!(
            early.blob_iter.evict_call_count(),
            1,
            "from_path_with_eviction(true) must evict from the header blob on"
        );

        // And the element stream is the same either way.
        let count = |reader: ElementReader<crate::read::file_reader::FileReader>| {
            let mut n = 0usize;
            reader.for_each(|_| n += 1).unwrap();
            n
        };
        let kept = count(ElementReader::from_path(path).unwrap());
        let evicted = count(ElementReader::from_path_with_eviction(path, true).unwrap());
        assert_eq!(kept, evicted, "eviction must not change what is delivered");
        assert!(kept > 0, "the fixture must contain elements");
    }

    #[test]
    fn sorted_history_allows_repeated_node_ids_in_both_iteration_paths() {
        use crate::block_builder::{BlockBuilder, HeaderBuilder, Metadata};
        use crate::writer::{Compression, PbfWriter};

        let mut pbf = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut pbf, Compression::None);
            let header = HeaderBuilder::new().sorted().historical().build().unwrap();
            writer.write_header(&header).unwrap();

            let first = Metadata {
                version: 1,
                timestamp: 1,
                changeset: 1,
                uid: 1,
                user: "history",
                visible: true,
            };
            let second = Metadata {
                version: 2,
                timestamp: 2,
                changeset: 2,
                uid: 1,
                user: "history",
                visible: true,
            };
            let mut block = BlockBuilder::new();
            block.add_node(1, 0, 0, std::iter::empty::<(&str, &str)>(), Some(&first));
            block.add_node(1, 0, 0, std::iter::empty::<(&str, &str)>(), Some(&second));
            writer
                .write_primitive_block(block.take().unwrap().unwrap())
                .unwrap();
            writer.flush().unwrap();
        }

        let mut sequential_count = 0;
        ElementReader::new(Cursor::new(pbf.clone()))
            .unwrap()
            .for_each(|_| sequential_count += 1)
            .unwrap();
        assert_eq!(sequential_count, 2);

        let mut pipelined_count = 0;
        ElementReader::new(Cursor::new(pbf))
            .unwrap()
            .decode_threads(1)
            .for_each_pipelined(|_| pipelined_count += 1)
            .unwrap();
        assert_eq!(pipelined_count, 2);
    }

    // The `par_map_reduce` frame pump admits blobs against `ByteBudget` before
    // reading further; these pin that bounded-admission contract deterministically
    // (no I/O, no threads-of-decode) so the reader working set stays bounded.

    #[test]
    fn byte_budget_blocks_at_cap_until_release() {
        let budget = Arc::new(ByteBudget::new(10));
        assert!(budget.acquire(6));

        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child = Arc::clone(&budget);
        let handle = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            // 6 + 5 > 10 and the budget is non-empty, so this blocks.
            let admitted = child.acquire(5);
            done_tx.send(admitted).unwrap();
        });

        ready_rx.recv().unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "acquire must block while the budget is over cap"
        );

        budget.release(6);
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "acquire must succeed once capacity frees up"
        );
        handle.join().unwrap();
    }

    #[test]
    fn byte_budget_admits_oversized_when_empty() {
        // A single legal blob larger than the whole cap must not deadlock: it is
        // admitted unconditionally when nothing else is in flight.
        let budget = ByteBudget::new(10);
        assert!(budget.acquire(100));
    }

    #[test]
    fn byte_budget_shutdown_unblocks_acquirer() {
        let budget = Arc::new(ByteBudget::new(10));
        assert!(budget.acquire(10));

        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child = Arc::clone(&budget);
        let handle = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let admitted = child.acquire(5);
            done_tx.send(admitted).unwrap();
        });

        ready_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());

        budget.shutdown();
        assert!(
            !done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "shutdown must wake a blocked acquirer and return false"
        );
        handle.join().unwrap();
    }

    #[test]
    fn batch_queue_drains_then_closes() {
        // pop() blocks on an empty open queue, so only pop after pushing/closing.
        let queue = BatchQueue::new();
        queue.push(Batch::new());
        queue.close();
        assert!(queue.pop().is_some(), "queued batch drains after close");
        assert!(queue.pop().is_none(), "closed empty queue returns None");
    }

    #[test]
    fn batch_queue_close_wakes_blocked_consumer() {
        let queue = Arc::new(BatchQueue::new());
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let child = Arc::clone(&queue);
        let handle = std::thread::spawn(move || {
            let got = child.pop();
            done_tx.send(got.is_none()).unwrap();
        });

        // Consumer is blocked on an empty, open queue.
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        queue.close();
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "close must wake a blocked consumer with None"
        );
        handle.join().unwrap();
    }

    // ---- par_map_reduce orchestration failure paths --------------------------
    //
    // These drive the whole worker-resident fold end to end - the queue/budget
    // unit tests above cannot expose the pump/worker deadlocks. Each risky path
    // runs under a watchdog: if a missing cancellation guard reintroduces a
    // deadlock, the watchdog panics with a clear message instead of hanging the
    // whole suite.

    /// Build an in-memory PBF: one header blob plus `data_blobs` OsmData blobs,
    /// four dense nodes each. Enough distinct blobs to exercise batching,
    /// budget backpressure, and mid-stream failures.
    fn build_pbf(data_blobs: usize) -> Vec<u8> {
        use crate::block_builder::{BlockBuilder, HeaderBuilder};
        use crate::writer::{Compression, PbfWriter};

        let mut buf = Vec::new();
        {
            let mut writer = PbfWriter::new(&mut buf, Compression::Zlib(6));
            let header = HeaderBuilder::new().build().unwrap();
            writer.write_header(&header).unwrap();
            for _ in 0..data_blobs {
                let mut bb = BlockBuilder::new();
                // Dense-node ids must ascend within a block; each block builder
                // starts fresh, so a local 1..=4 range per block is fine. Blob
                // content need not differ - only the blob count matters here.
                for i in 1..=4_i32 {
                    bb.add_node(
                        i64::from(i),
                        500_000_000 + i,
                        100_000_000 + i,
                        std::iter::empty::<(&str, &str)>(),
                        None,
                    );
                }
                let block = bb.take().unwrap().unwrap();
                writer.write_primitive_block(block).unwrap();
            }
            writer.flush().unwrap();
        }
        buf
    }

    /// Byte offset where the first OsmData blob begins - i.e. the point the
    /// frame pump starts reading after the header blob was consumed in
    /// `ElementReader::new`.
    fn first_data_blob_offset(buf: &[u8]) -> usize {
        use crate::read::blob::BlobReader;
        let mut r = BlobReader::new_seekable(Cursor::new(buf.to_vec())).unwrap();
        let _header = r.next().unwrap().unwrap();
        let first_data = r.next().unwrap().unwrap();
        usize::try_from(first_data.offset().unwrap().0).unwrap()
    }

    /// Run `f` on a helper thread and fail loudly if it does not finish within a
    /// generous window - a deadlock would otherwise hang the whole test binary.
    /// Returns the thread result so callers can assert an expected panic.
    #[allow(clippy::unwrap_in_result)]
    fn assert_completes<F, T>(label: &str, f: F) -> std::thread::Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel::<()>(1);
        let handle = std::thread::spawn(move || {
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            if tx.send(()).is_err() {
                // Watchdog already timed out and dropped the receiver; the
                // panic below has fired and this result is unobserved.
            }
            out
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) => handle
                .join()
                .expect("watchdog thread itself failed to join"),
            Err(_) => panic!("{label}: did not complete within timeout - deadlock"),
        }
    }

    /// A `Read` that serves `data[..panic_at]` then panics on any further read,
    /// simulating an I/O source that faults mid-stream inside the frame pump.
    struct PanicAfterRead {
        data: Vec<u8>,
        pos: usize,
        panic_at: usize,
    }

    impl Read for PanicAfterRead {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            assert!(
                self.pos <= self.panic_at,
                "reader served past its panic threshold"
            );
            if self.pos >= self.panic_at {
                panic!("boom in Read during pump");
            }
            let end = (self.pos + out.len()).min(self.panic_at);
            let n = end - self.pos;
            out[..n].copy_from_slice(&self.data[self.pos..end]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn par_map_reduce_accepts_borrowed_non_static_reader() {
        // The whole reason par_map_reduce uses scoped threads: it must accept a
        // reader borrowing local data (no `R: 'static`). `Cursor<&[u8]>` borrows
        // `buf`, so this only compiles if the bound really is absent - and the
        // count proves the borrowed path decodes correctly.
        let buf = build_pbf(3);
        let reader = ElementReader::new(Cursor::new(buf.as_slice())).unwrap();
        let count = reader
            .par_map_reduce(|_e| 1_u64, || 0_u64, |a, b| a + b)
            .unwrap();
        assert_eq!(count, 12, "3 blobs x 4 dense nodes");
    }

    #[test]
    fn pump_budget_rule_refuses_a_budget_its_own_batch_can_fill() {
        use super::pump_cannot_wait_on_own_batch;
        use crate::read::blob_wire::MAX_BLOB_DATASIZE;
        // A one-blob batch never holds capacity at an acquire.
        assert!(pump_cannot_wait_on_own_batch(1, 1, 4096));
        // A multi-blob batch needs room for the largest partial batch (a lone
        // maximum-size blob here) plus one more maximum-size blob.
        assert!(pump_cannot_wait_on_own_batch(
            2 * MAX_BLOB_DATASIZE,
            2,
            4096
        ));
        assert!(!pump_cannot_wait_on_own_batch(
            2 * MAX_BLOB_DATASIZE - 1,
            2,
            4096
        ));
        // A byte target above the blob limit is what the partial batch holds.
        assert!(!pump_cannot_wait_on_own_batch(
            3 * MAX_BLOB_DATASIZE,
            2,
            3 * MAX_BLOB_DATASIZE
        ));
    }

    #[test]
    fn worker_panic_one_worker_over_budget_does_not_deadlock() {
        // decode_threads(1) + a 1-byte budget: the pump admits blob 0 then
        // blocks in `acquire`, and the single worker panics folding blob 0.
        // Without the worker cancel guard (release + shutdown), the pump would
        // block in `acquire` forever and never reach the join loop.
        let buf = build_pbf(4);
        let result = assert_completes("worker panic", move || {
            let ElementReader { blob_iter, .. } = ElementReader::new(Cursor::new(buf)).unwrap();
            par_fold_blobs(
                blob_iter,
                1, // one worker
                1, // tiny budget: pump blocks after blob 0
                1, // one blob per batch: deliver eagerly
                super::PAR_BATCH_MAX_BYTES,
                |_e| -> u64 { panic!("boom in map_op") },
                || 0_u64,
                |a, b| a + b,
            )
        });
        assert!(
            result.is_err(),
            "a worker panic must propagate as a panic, not be swallowed"
        );
    }

    #[test]
    fn panicking_read_during_pump_does_not_deadlock() {
        // A Read that faults on the pump's first data-blob read. The calling
        // thread unwinds inside the scope with workers blocked on the open
        // queue; the calling-thread cancel guard must close the queue so
        // scope-join completes and the panic propagates.
        let buf = build_pbf(3);
        let panic_at = first_data_blob_offset(&buf);
        let result = assert_completes("panicking read", move || {
            let reader = ElementReader::new(PanicAfterRead {
                data: buf,
                pos: 0,
                panic_at,
            })
            .unwrap();
            reader.par_map_reduce(|_e| 1_u64, || 0_u64, |a, b| a + b)
        });
        assert!(
            result.is_err(),
            "a panicking Read must propagate, not deadlock scope cleanup"
        );
    }

    #[test]
    fn read_error_wins_over_simultaneous_decode_error() {
        // Corrupt the sole data blob's zlib trailer (a decode error once a
        // worker touches it) and append a framing (read) error right after it.
        // The pump reads the bad frame; with batch_max_blobs = 1 the corrupt
        // blob is already in flight to a worker, so both errors race. The
        // framing error must win deterministically.
        let mut buf = build_pbf(1);
        let n = buf.len();
        // The zlib stream's 4-byte adler32 trailer ends the file; flipping it
        // guarantees an inflate failure while leaving all framing/lengths intact.
        for b in &mut buf[n - 4..n] {
            *b ^= 0xFF;
        }
        // A length prefix declaring a BlobHeader >= MAX_BLOB_HEADER_SIZE makes
        // the pump's next read a hard HeaderTooBig framing error.
        buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]); // 65536 == MAX

        let result = assert_completes("read vs decode", move || {
            let ElementReader { blob_iter, .. } = ElementReader::new(Cursor::new(buf)).unwrap();
            par_fold_blobs(
                blob_iter,
                2,
                super::PAR_INFLIGHT_BUDGET,
                1, // deliver the corrupt blob to a worker before the bad frame
                super::PAR_BATCH_MAX_BYTES,
                |_e| 1_u64,
                || 0_u64,
                |a, b| a + b,
            )
        })
        .expect("orchestration must not panic");
        let err = result.expect_err("must surface an error");
        assert!(
            matches!(err.kind(), ErrorKind::Blob(BlobError::HeaderTooBig { .. })),
            "framing/read error must win over the decode error, got {err:?}"
        );
    }

    /// A pipeline thread that dies without finishing must surface as an error,
    /// not as a clean end of file.
    ///
    /// `PipelinedBlocks::next` receives from a channel whose only sender lives
    /// on the pipeline thread, so a panic there closes the channel exactly as
    /// normal completion does. Mapping the receive failure straight to `None`
    /// therefore reported a crashed pipeline as "the file ended", which is
    /// silent data loss with a zero exit code. The `completed` flag is what
    /// tells the two apart.
    ///
    /// Both halves are asserted: a thread that completes normally must still
    /// end iteration with `None`, or this test would pass against an iterator
    /// that always reports an error.
    #[test]
    fn a_panicking_pipeline_thread_is_not_reported_as_end_of_file() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let build = |panics: bool| {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let completed = Arc::new(AtomicBool::new(false));
            let thread_completed = Arc::clone(&completed);
            let handle = std::thread::spawn(move || {
                let _tx = tx;
                if panics {
                    // Emulate an unwind on the pipeline thread: the sender is
                    // dropped without `completed` ever being set.
                    panic!("pipeline thread died");
                }
                thread_completed.store(true, Ordering::Release);
            });
            let call = crate::host::Call::detached();
            PipelinedBlocks {
                rx: Some(rx),
                handle: Some(handle),
                completed,
                _ticket: call.ticket(crate::host::Kind::Session),
                invocation: call.handle().clone(),
            }
        };

        // Silence the panic backtrace this test deliberately provokes.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut crashed = build(true);
        let item = crashed.next();
        std::panic::set_hook(previous);

        let err = item
            .expect("a dead pipeline must yield an item, not end-of-iteration")
            .expect_err("and that item must be an error");
        assert!(
            format!("{err}").contains("panicked): pipeline thread died"),
            "expected the pipeline-death error, got: {err}"
        );
        assert!(
            crashed.next().is_none(),
            "the error must be reported once, then iteration ends"
        );

        let mut clean = build(false);
        assert!(
            clean.next().is_none(),
            "a pipeline that completed must end iteration cleanly"
        );

        // The two halves above exercise the iterator against a hand-built
        // channel. This one pins the wiring: the real pipeline thread must
        // actually set the flag, or every pipelined read in the crate would end
        // with the spurious error asserted above.
        let real = ElementReader::from_path("tests/test.osm.pbf")
            .unwrap()
            .into_blocks_pipelined()
            .unwrap();
        let mut blocks = 0;
        for block in real {
            block.expect("a well-formed file must not yield a pipeline error");
            blocks += 1;
        }
        assert!(
            blocks > 0,
            "the fixture must deliver at least one block, or the loop above \
             asserts nothing"
        );

        // Early termination must stay clean too: dropping the iterator part-way
        // is a supported use (zipping, `take`), and must not turn into an error
        // on the next call.
        let mut partial = ElementReader::from_path("tests/test.osm.pbf")
            .unwrap()
            .into_blocks_pipelined()
            .unwrap();
        partial
            .next()
            .expect("at least one block")
            .expect("first block must decode");
        drop(partial);
    }
}
