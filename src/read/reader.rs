//! High level reader interface

use super::blob::{Blob, BlobDecode, BlobReader, BlobType};
use super::elements::Element;
use crate::error::Result;
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

/// A reader for PBF files that gives access to the stored elements: nodes, ways and relations.
#[derive(Clone, Debug)]
pub struct ElementReader<R: Read + Send> {
    blob_iter: BlobReader<R>,
}

impl<R: Read + Send> ElementReader<R> {
    /// Creates a new `ElementReader`.
    ///
    /// # Example
    /// ```
    /// use pbfhogg::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let reader = ElementReader::new(buf_reader);
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new(reader: R) -> ElementReader<R> {
        ElementReader {
            blob_iter: BlobReader::new(reader),
        }
    }

    /// Decodes the PBF structure sequentially and calls the given closure on each element.
    /// Consider using `par_map_reduce` instead if you need better performance.
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
        for blob in self.blob_iter {
            match blob?.decode() {
                Ok(BlobDecode::OsmHeader(_)) | Ok(BlobDecode::Unknown(_)) => {}
                Ok(BlobDecode::OsmData(block)) => {
                    block.for_each_element(&mut f);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    /// Decodes the PBF structure using a pipelined approach and calls the given closure on each
    /// element, preserving file order. Overlaps I/O with parallel decompression and protobuf
    /// parsing while delivering elements to an `FnMut` closure on the calling thread.
    #[hotpath::measure]
    pub fn for_each_pipelined<F>(self, mut f: F) -> Result<()>
    where
        F: for<'a> FnMut(Element<'a>),
    {
        super::pipeline::run_pipeline(self.blob_iter, |block| {
            block.for_each_element(&mut f);
            Ok(())
        })
    }

    /// Parallel map/reduce. Decodes the PBF structure in parallel, calls the closure `map_op` on
    /// each element and then reduces the number of results to one item with the closure
    /// `reduce_op`. Similarly to the `init` argument in the `fold` method on iterators, the
    /// `identity` closure should produce an identity value that is inserted into `reduce_op` when
    /// necessary. The number of times that this identity value is inserted should not alter the
    /// result.
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
    pub fn par_map_reduce<MP, RD, ID, T>(self, map_op: MP, identity: ID, reduce_op: RD) -> Result<T>
    where
        MP: for<'a> Fn(Element<'a>) -> T + Sync + Send,
        RD: Fn(T, T) -> T + Sync + Send,
        ID: Fn() -> T + Sync + Send,
        T: Send,
    {
        // Phase 1: Sequentially collect all OsmData blobs into a Vec.
        // Blobs are still compressed at this stage (~16-64KB each), so the Vec
        // holds only the compressed data. Header and Unknown blobs are skipped
        // since they don't contain map-reducible elements.
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

impl ElementReader<BufReader<File>> {
    /// Tries to open the file at the given path and constructs an `ElementReader` from this.
    ///
    /// # Errors
    /// Returns the same errors that `std::fs::File::open` returns.
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
        Ok(ElementReader {
            blob_iter: BlobReader::from_path(path)?,
        })
    }
}

/// Sequentially iterate a `BlobReader`, collecting all OsmData blobs into a Vec.
///
/// Skips header blobs and unknown blob types since they contain no OSM elements.
/// Returns early on the first I/O or parse error from the blob reader.
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
