//! High level reader interface

use super::blob::{Blob, BlobDecode, BlobReader, BlobType};
use super::block::{HeaderBlock, PrimitiveBlock};
use super::elements::Element;
use super::file_reader::FileReader;
use super::pipeline::PipelineConfig;
use crate::blob_index::BlobFilter;
use crate::error::{new_error, ErrorKind, Result};
use rayon::prelude::*;
use std::io::Read;
use std::path::Path;
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread::JoinHandle;

/// Number of decoded blocks buffered between the pipeline and the consumer iterator.
const BLOCK_QUEUE: usize = 8;

/// A reader for PBF files that gives access to the stored elements: nodes, ways and relations.
///
/// The PBF header is parsed eagerly at construction time and is accessible via [`header()`](Self::header).
// wontfix(type-generic-bounds): bounds on struct match osmpbf API and document intent
#[derive(Clone, Debug)]
pub struct ElementReader<R: Read + Send> {
    blob_iter: BlobReader<R>,
    header: HeaderBlock,
    decode_threads: Option<usize>,
    pipeline_config: PipelineConfig,
    blob_filter: Option<BlobFilter>,
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
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
        })
    }

    /// Sets a blob-type filter for the pipelined reader.
    ///
    /// When set, the pipeline skips decompressing blobs whose element type
    /// (from indexdata) does not match the filter. For PBFs without indexdata,
    /// all blobs pass through unchanged.
    ///
    /// This dramatically reduces CPU usage for type-filtered commands: e.g.
    /// filtering for ways only skips decompressing ~85% of blobs (nodes).
    pub fn with_blob_filter(mut self, filter: BlobFilter) -> Self {
        self.blob_filter = Some(filter);
        self
    }

    /// Sets the number of threads in the decode pool used by
    /// [`for_each_pipelined`](Self::for_each_pipelined),
    /// [`for_each_block_pipelined`](Self::for_each_block_pipelined), and
    /// [`into_blocks_pipelined`](Self::into_blocks_pipelined).
    ///
    /// When not set, defaults to `available_parallelism() - 2` (reserving threads
    /// for the I/O reader and the consumer). The minimum is clamped to 1.
    pub fn decode_threads(mut self, n: usize) -> Self {
        self.decode_threads = Some(n.max(1));
        self
    }

    /// Sets Stage 1 pipeline read-ahead depth (raw blobs buffered between I/O and decode).
    ///
    /// Defaults to 16. Values <1 are clamped to 1.
    pub fn read_ahead(mut self, n: usize) -> Self {
        self.pipeline_config.read_ahead = n.max(1);
        self
    }

    /// Sets Stage 2 pipeline decode-ahead depth.
    ///
    /// Controls both the channel capacity between the decode pool and the
    /// reorder buffer, and the reorder buffer's own capacity. Lower values
    /// reduce memory usage; higher values absorb decode-time variance.
    ///
    /// Defaults to 32. Values <1 are clamped to 1.
    pub fn decode_ahead(mut self, n: usize) -> Self {
        self.pipeline_config.decode_ahead = n.max(1);
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

    /// Decodes the PBF structure sequentially on the calling thread - no background I/O,
    /// no rayon, no channels. Elements are delivered in file order. If
    /// [`header().is_sorted()`](HeaderBlock::is_sorted) returns `true`, nodes are guaranteed
    /// to arrive in ascending ID order.
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
    pub fn for_each<F>(self, mut f: F) -> Result<()>
    where
        F: for<'a> FnMut(Element<'a>),
    {
        let Self { blob_iter, header, .. } = self;
        let is_sorted = header.is_sorted();
        let mut last_node_id: i64 = i64::MIN;

        for blob in blob_iter {
            match blob?.decode() {
                Ok(BlobDecode::OsmData(block)) => {
                    block.for_each_element(|element| {
                        if is_sorted
                            && let Some(id) = node_id(&element)
                        {
                            debug_assert!(
                                id > last_node_id,
                                "Sort.Type_then_ID violated: node {id} <= previous {last_node_id}"
                            );
                            last_node_id = id;
                        }
                        f(element);
                    });
                }
                Ok(_) => {} // Unknown blobs - header already consumed at construction
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Decodes the PBF structure using a pipelined approach and calls the given closure on each
    /// element, preserving file order. Overlaps I/O with parallel decompression and protobuf
    /// parsing while delivering elements to an `FnMut` closure on the calling thread.
    ///
    /// Elements are delivered in file order. If [`header().is_sorted()`](HeaderBlock::is_sorted)
    /// returns `true`, nodes are guaranteed to arrive in ascending ID order.
    #[hotpath::measure]
    pub fn for_each_pipelined<F>(self, mut f: F) -> Result<()>
    where
        F: for<'a> FnMut(Element<'a>),
    {
        let is_sorted = self.header.is_sorted();
        let mut last_node_id: i64 = i64::MIN;

        self.for_each_block_pipelined(|block| {
            block.for_each_element(|element| {
                if is_sorted
                    && let Some(id) = node_id(&element)
                {
                    debug_assert!(
                        id > last_node_id,
                        "Sort.Type_then_ID violated: node {id} <= previous {last_node_id}"
                    );
                    last_node_id = id;
                }
                f(element);
            });
            Ok(())
        })
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
        super::pipeline::run_pipeline(
            self.blob_iter,
            self.decode_threads,
            self.pipeline_config,
            self.blob_filter,
            f,
        )
    }

    /// Returns an iterator of decoded [`PrimitiveBlock`]s from the pipelined reader.
    ///
    /// The 3-stage pipeline (I/O → decode → reorder) runs in a background thread.
    /// Blocks arrive in file order via a bounded channel. The consumer controls
    /// the iteration pace; backpressure propagates naturally when the channel fills.
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
    pub fn into_blocks_pipelined(self) -> PipelinedBlocks
    where
        R: 'static,
    {
        let (tx, rx) = sync_channel(BLOCK_QUEUE);
        let blob_iter = self.blob_iter;
        let decode_threads = self.decode_threads;
        let pipeline_config = self.pipeline_config;
        let blob_filter = self.blob_filter;

        let handle = std::thread::spawn(move || {
            let result = super::pipeline::run_pipeline(
                blob_iter,
                decode_threads,
                pipeline_config,
                blob_filter,
                |block| {
                    tx.send(Ok(block)).map_err(|_| {
                        new_error(ErrorKind::Io(std::io::Error::other(
                            "pipeline consumer dropped",
                        )))
                    })
                },
            );
            if let Err(e) = result {
                // Deliver the error as the last iterator item.
                // Ignore send failure - consumer may have already dropped.
                drop(tx.send(Err(e)));
            }
        });

        PipelinedBlocks {
            rx: Some(rx),
            handle: Some(handle),
        }
    }

    /// Parallel map/reduce. Decodes the PBF structure in parallel, calls the closure `map_op` on
    /// each element and then reduces the number of results to one item with the closure
    /// `reduce_op`. Similarly to the `init` argument in the `fold` method on iterators, the
    /// `identity` closure should produce an identity value that is inserted into `reduce_op` when
    /// necessary. The number of times that this identity value is inserted should not alter the
    /// result.
    ///
    /// **Note:** Elements are delivered in arbitrary order across rayon worker threads.
    /// The [`Sort.Type_then_ID`](HeaderBlock::is_sorted) ordering guarantee does **not**
    /// apply to this method. Use [`for_each`](Self::for_each) or
    /// [`for_each_pipelined`](Self::for_each_pipelined) if you need sorted element order.
    ///
    /// # Memory
    ///
    /// This method collects **all** compressed blobs into memory before parallel
    /// processing. Memory usage is approximately equal to the PBF file size
    /// (compressed blobs are ~16-64 KB each). For a planet file (~80 GB), this
    /// requires ~80 GB of RAM for the blob collection alone, plus one decoded
    /// block (~1.4 MB) per rayon worker thread.
    ///
    /// For memory-constrained environments processing large files, use
    /// [`for_each_pipelined`](Self::for_each_pipelined) instead, which streams
    /// blocks through a bounded channel with constant memory overhead.
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
    //
    // ## Implementation: batch-collect + into_par_iter
    //
    // ### Why not par_bridge()?
    //
    // The previous implementation used `par_bridge()` to parallelize iteration
    // over the sequential `BlobReader`. Rayon's `par_bridge()` wraps the
    // sequential iterator with a `Mutex`, and every rayon worker thread must
    // acquire that lock to pull the next item. At high parallelism (8+ cores),
    // this single-lock contention becomes a significant bottleneck: threads
    // spend time spinning/blocking on the mutex instead of doing useful decode
    // work. Profiling shows this contention dominates at high core counts,
    // limiting scalability.
    //
    // ### Why batch-collect is better
    //
    // Instead, we split the work into two phases:
    //
    //   Phase 1 (sequential): Iterate the BlobReader and collect all OsmData
    //   blobs into a Vec<Blob>. This is cheap because blobs at this stage are
    //   still compressed -- typically 16-64KB each. The I/O and collection is
    //   inherently sequential (single stream), but the per-blob cost is just
    //   reading + a small protobuf header parse, no decompression.
    //
    //   Phase 2 (parallel): Use `into_par_iter()` on the Vec for lock-free
    //   parallel decode + map + reduce. Rayon splits the Vec into contiguous
    //   chunks and assigns them to worker threads with zero synchronization
    //   overhead -- no mutex, no atomic contention. Each thread independently
    //   decompresses, parses protobuf, and applies the map/reduce closures.
    //
    // The expensive work (zlib decompression + protobuf parsing, typically
    // 500KB-2MB of decompressed data per blob) happens entirely in Phase 2,
    // which is fully parallel and lock-free.
    //
    // ### Memory safety analysis
    //
    // Collecting all compressed blobs into a Vec is safe for memory:
    //   - A ~500MB PBF file (e.g. Germany) has ~16K blobs at ~32KB avg
    //     compressed = ~512MB in the Vec. This is comparable to the file size
    //     itself and well within typical system memory.
    //   - The full planet (~80GB PBF) has ~2.5M blobs at ~32KB avg = ~80GB.
    //     This is the same order as the file size, and any system processing
    //     the planet file already needs substantial RAM for the decoded data.
    //   - The Vec is consumed by `into_par_iter()` and each Blob is dropped
    //     after processing, so peak memory is the Vec plus the in-flight
    //     decoded blocks (one per rayon thread).
    //
    // ### Alternatives considered
    //
    // - **Chunked collection** (collect N blobs, process, repeat): Would cap
    //   memory but adds complexity and reintroduces synchronization between
    //   chunks. For typical PBF sizes the full collect is fine.
    //
    // - **Channel-based producer/consumer** (crossbeam channel feeding rayon):
    //   More complex, introduces backpressure tuning. The pipelined reader
    //   (`for_each_pipelined`) already provides this pattern for ordered
    //   processing; par_map_reduce is for unordered reduce where batch-collect
    //   is simpler and faster.
    //
    // - **par_bridge() with larger work stealing granularity**: Rayon doesn't
    //   expose per-bridge granularity controls. The mutex is fundamental to
    //   how par_bridge adapts a sequential iterator.
    //
    pub fn par_map_reduce<MP, RD, ID, T>(mut self, map_op: MP, identity: ID, reduce_op: RD) -> Result<T>
    where
        MP: for<'a> Fn(Element<'a>) -> T + Sync + Send,
        RD: Fn(T, T) -> T + Sync + Send,
        ID: Fn() -> T + Sync + Send,
        T: Send,
    {
        // Phase 1: Sequentially collect all OsmData blobs into a Vec.
        // Blobs are still compressed at this stage (~16-64KB each), so the Vec
        // holds only the compressed data. The header blob was already consumed
        // at construction time, so only data and unknown blobs remain.
        // Skip indexdata parsing - par_map_reduce never calls blob.index().
        self.blob_iter.set_parse_indexdata(false);
        let blobs = collect_osm_data_blobs(self.blob_iter)?;

        // Phase 2: Parallel decode + map + reduce with zero lock contention.
        // Rayon's into_par_iter() splits the Vec into contiguous slices for each
        // worker thread -- no mutex, no atomic CAS, just index arithmetic.
        blobs
            .into_par_iter()
            .try_fold(
                &identity,
                |acc, blob: Blob| match blob.decode()? {
                    BlobDecode::OsmData(block) => {
                        Ok(block.elements().map(&map_op).fold(acc, &reduce_op))
                    }
                    // Should not happen: collect_osm_data_blobs filters to OsmData only.
                    // Handle gracefully by returning the accumulator unchanged.
                    BlobDecode::OsmHeader(_) | BlobDecode::Unknown(_) => Ok(acc),
                },
            )
            .try_reduce(&identity, |a, b| Ok(reduce_op(a, b)))
    }
}

impl ElementReader<FileReader> {
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
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
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
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
        })
    }

    /// Open a file, selecting buffered or O_DIRECT based on the `direct` flag.
    pub fn open<P: AsRef<Path>>(path: P, direct: bool) -> Result<Self> {
        let mut blob_iter = BlobReader::open(path, direct)?;
        let header = read_header_blob(&mut blob_iter)?;
        Ok(ElementReader {
            blob_iter,
            header,
            decode_threads: None,
            pipeline_config: PipelineConfig::default(),
            blob_filter: None,
        })
    }
}

/// Read and parse the header blob from a `BlobReader`.
///
/// Consumes the first blob from the reader. Returns an error if there are no blobs
/// or the first blob is not an `OsmHeader`.
fn read_header_blob<R: Read + Send>(blob_iter: &mut BlobReader<R>) -> Result<HeaderBlock> {
    match blob_iter.next() {
        Some(Ok(blob)) => match blob.decode()? {
            BlobDecode::OsmHeader(header) => Ok(*header),
            _ => Err(new_error(ErrorKind::MissingHeader)),
        },
        Some(Err(e)) => Err(e),
        None => Err(new_error(ErrorKind::MissingHeader)),
    }
}

/// Extract the node ID from an element, if it is a node.
fn node_id(element: &Element<'_>) -> Option<i64> {
    match element {
        Element::Node(n) => Some(n.id()),
        Element::DenseNode(n) => Some(n.id()),
        _ => None,
    }
}

/// Sequentially iterate a `BlobReader`, collecting all OsmData blobs into a Vec.
///
/// Skips unknown blob types since they contain no OSM elements. The header blob
/// has already been consumed at `ElementReader` construction time.
///
/// This is used by `par_map_reduce` to separate the sequential I/O phase from
/// the parallel decode phase. See the comments on `par_map_reduce` for the full
/// rationale.
#[hotpath::measure]
fn collect_osm_data_blobs<R: Read + Send>(blob_iter: BlobReader<R>) -> Result<Vec<Blob>> {
    let mut blobs = Vec::new();
    for blob_result in blob_iter {
        let blob = blob_result?;
        if blob.get_type() == BlobType::OsmData {
            blobs.push(blob);
        }
    }
    Ok(blobs)
}

// ---------------------------------------------------------------------------
// PipelinedBlocks iterator
// ---------------------------------------------------------------------------

/// Iterator over decoded [`PrimitiveBlock`]s from a pipelined PBF reader.
///
/// Created by [`ElementReader::into_blocks_pipelined`]. The 3-stage pipeline
/// runs in a background thread; blocks are delivered in file order via a bounded
/// channel. Dropping this iterator signals the pipeline to shut down.
pub struct PipelinedBlocks {
    rx: Option<Receiver<Result<PrimitiveBlock>>>,
    handle: Option<JoinHandle<()>>,
}

impl Iterator for PipelinedBlocks {
    type Item = Result<PrimitiveBlock>;

    fn next(&mut self) -> Option<Self::Item> {
        self.rx.as_ref()?.recv().ok()
    }
}

impl Drop for PipelinedBlocks {
    fn drop(&mut self) {
        // Close the channel first - signals the pipeline to shut down.
        drop(self.rx.take());
        // Join the background thread (waits for pipeline cleanup).
        if let Some(h) = self.handle.take() {
            drop(h.join());
        }
    }
}
