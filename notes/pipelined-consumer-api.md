# Pipelined consumer API: block-level ownership for parallel consumers

## Problem

`for_each_pipelined` yields `Element<'a>` one at a time to an `FnMut` callback on
the main thread. The element borrows from a `PrimitiveBlock` that the pipeline owns.
The consumer can't send elements or blocks to other threads.

Consumers that need to do parallel work (elivagar's way processing) must:
1. Extract/copy data out of each element into a batch buffer
2. When the batch is full, block the main thread while rayon processes it
3. Resume receiving elements

This creates a bubble: the main thread alternates between collecting (rayon idle)
and waiting on rayon (pipeline stalled). elivagar reports 57% of PBF read time
spent blocked on rayon, with pipeline decode threads idle during collection phases.

```
Main:  [collect][--wait on rayon--][collect][--wait on rayon--]
Rayon: [--idle--][  process batch  ][--idle--][  process batch  ]
```

The consumer-side fix (double-buffered batches in elivagar) works but requires
every consumer to reinvent the same pattern. The library can help.

## Current architecture

```
Stage 1: I/O thread        ──sync_channel(16)──▶  Stage 2: Decode pool (N threads)
                                                           │
Stage 3: Main thread  ◀──sync_channel(32)──  reorder buffer (VecDeque)
                │
         block_fn(&PrimitiveBlock)     ← borrows from reorder buffer
                │
         for_each_element(|element| f(element))   ← borrows from block
```

Key observation: the reorder buffer already **owns** each PrimitiveBlock. It
`pop_front()`s to take ownership, then artificially borrows with `ref block`:

```rust
let item = pending.pop_front().unwrap().unwrap();
match item {
    Some(Ok(ref block)) => block_fn(block)?,   // borrow from owned value
    ...
}
```

Removing `ref` gives the consumer an owned PrimitiveBlock for free.

## API options

### Option A: Block-level owned callback

```rust
pub fn for_each_block_pipelined<F>(self, f: F) -> Result<()>
where
    F: FnMut(PrimitiveBlock) -> Result<()>,
```

**Change to run_pipeline:** Remove `ref` from the match arm. Change `block_fn`
signature from `FnMut(&PrimitiveBlock)` to `FnMut(PrimitiveBlock)`.

**Change to for_each_pipelined:** Becomes a thin wrapper:
```rust
pub fn for_each_pipelined<F>(self, mut f: F) -> Result<()>
where
    F: for<'a> FnMut(Element<'a>),
{
    let is_sorted = self.header.is_sorted();
    let mut last_node_id = i64::MIN;
    super::pipeline::run_pipeline(self.blob_iter, |block| {
        block.for_each_element(|element| {
            // sorted assertion ...
            f(element);
        });
        Ok(())
    })
}
```

**Consumer pattern (elivagar):**
```rust
let (way_tx, way_rx) = crossbeam::channel::bounded(2);  // double-buffer

// Spawn way processing thread
let processor = std::thread::spawn(move || {
    for block in way_rx {
        rayon_process_ways(block, &node_store_reader);
    }
});

reader.for_each_block_pipelined(|block| {
    match classify_block(&block) {
        Nodes => {
            // Process inline — fast, sequential
            for dn in block.dense_nodes() {
                node_store.put(dn);
            }
        }
        Ways => {
            // Send owned block to processing thread — non-blocking
            way_tx.send(block)?;
        }
        Relations => { ... }
    }
    Ok(())
})?;
```

The consumer sends entire blocks to a processing thread. The pipeline keeps
delivering blocks to the main thread without blocking. When the bounded channel
is full (2 blocks in flight), the main thread blocks — but only briefly, since
way processing is faster than the pipeline can deliver.

**Advantages:**
- Minimal implementation change (~5 lines in pipeline.rs, new method in reader.rs)
- No `'static` bound — works with any `R: Read + Send`
- Backward compatible (new method, existing API unchanged)
- No performance cost (block was going to be dropped at end of closure anyway)
- Consumer chooses their own parallelism model (channels, rayon, threads)

**Limitations:**
- Consumer must still match on block contents to decide node vs way vs relation
- Sending entire blocks is coarser than batching individual elements
- If consumer needs cross-block batching (batch across multiple PBF blocks), they
  still need to extract data

### Option B: Block iterator

```rust
pub fn into_blocks_pipelined(self) -> PipelinedBlocks
// PipelinedBlocks: Iterator<Item = Result<PrimitiveBlock>>
```

Consumer pulls blocks at their own pace from a standard Iterator.

**Implementation:** Run the 3-stage pipeline in a background thread. Stage 3
sends owned blocks through a channel instead of calling block_fn. The iterator
pulls from the receiver.

```rust
pub struct PipelinedBlocks {
    rx: Receiver<Result<PrimitiveBlock>>,
    _handle: JoinHandle<Result<()>>,
}

impl Iterator for PipelinedBlocks {
    type Item = Result<PrimitiveBlock>;
    fn next(&mut self) -> Option<Self::Item> {
        self.rx.recv().ok()
    }
}
```

**Consumer pattern:**
```rust
let blocks = reader.into_blocks_pipelined();
for block_result in blocks {
    let block = block_result?;
    // Full control: batch, dispatch, interleave, whatever
}
```

**Advantages:**
- Maximum flexibility — consumer controls the loop
- Natural for complex multi-phase processing
- Works with for loops, itertools, etc.
- No callback nesting

**Limitations:**
- Requires `R: Read + Send + 'static` (background thread can't borrow)
  - `ElementReader<FileReader>` satisfies this (the common case)
  - `ElementReader<BufReader<&File>>` does not (rare, test-only)
- Pipeline lifecycle management: Drop must join background threads
- One extra channel hop vs callback (negligible cost, ~50ns per block)
- Error handling: pipeline errors arrive via the iterator, not at call site

### Option C: Parallel block map (4th pipeline stage)

```rust
pub fn map_blocks_pipelined<M, T>(self, map: M) -> Result<Vec<T>>
where
    M: Fn(&PrimitiveBlock) -> T + Send + Sync,
    T: Send,
```

Built-in parallel processing stage: decode pool → parallel user map → collect.

**Implementation:** After the reorder buffer delivers a block in order, dispatch
it to the global rayon pool for the user's map function. Collect results in order.

**Advantages:**
- Turnkey parallel processing, no consumer-side threading
- Results arrive in file order

**Limitations:**
- Too opinionated: doesn't fit elivagar's node-then-way pattern
- Can't have mutable state in the map (Fn, not FnMut)
- Collects all results into Vec (memory)
- Different consumers need different parallelism granularity

**Verdict:** Not general enough. Skip.

### Option D: for_each with send-able element data

Add methods to extract owned data from elements:

```rust
impl Way<'_> {
    pub fn to_owned_tags(&self) -> Vec<(String, String)> { ... }
    pub fn to_owned_refs(&self) -> Vec<i64> { ... }
}
```

**Verdict:** This is what consumers already do manually. Adding convenience methods
doesn't solve the architectural problem (main thread still blocks during rayon).
Orthogonal to the pipeline API — could be added separately.

## Recommendation

**Both Option A and Option B are implemented.**

Option A (`for_each_block_pipelined`) — commit `59fe13d`:
- 1 modified function (run_pipeline: `&PrimitiveBlock` → owned `PrimitiveBlock`)
- 1 new public method (`for_each_block_pipelined`)
- Existing `for_each_pipelined` becomes a wrapper (no behavior change)
- Unblocks elivagar's double-buffer pattern immediately

Option B (`into_blocks_pipelined`) — commit `7a884dc`:
- `PipelinedBlocks` struct implementing `Iterator<Item = Result<PrimitiveBlock>>`
- Pipeline runs in background thread, blocks delivered via `sync_channel(8)`
- Requires `R: 'static` (`ElementReader<FileReader>` satisfies this)
- Enables loop control: early exit, zipping two PBF iterators, interleaving work
- Clean Drop: closes channel first (signals shutdown), then joins background thread

## Elivagar impact estimate

With the owned-block API + double-buffered processing:

```
Before:
  Main:  [collect 5.7s][--wait 7.6s--][collect][--wait--]...
  Rayon: [---idle 5.7s---][process 7.6s][--idle--][process]

After:
  Main:  [collect][collect][collect][collect]...   ← never blocks
  Rayon: [process][process][process][process]...   ← never idle
  Pipeline decode threads: running continuously
```

The 5.7s collection and 7.6s processing overlap fully. Wall time drops from
~13.3s to ~max(7.6s, 5.7s + pipeline overhead) ≈ 8s. ~40% improvement on the
PBF read phase.

In practice the improvement depends on whether the decode pool can keep up (it
currently does — the main thread callback was the bottleneck).

## Implementation sketch (Option A)

### pipeline.rs

```rust
// Change block_fn signature from &PrimitiveBlock to PrimitiveBlock
pub(crate) fn run_pipeline<R, F>(blob_reader: BlobReader<R>, mut block_fn: F) -> Result<()>
where
    R: Read + Send,
    F: FnMut(PrimitiveBlock) -> Result<()>,   // owned, not borrowed
{
    // ... Stages 1 and 2 unchanged ...

    // Stage 3: change one line in the drain loop
    match item {
        Some(Ok(block)) => block_fn(block)?,   // was: Some(Ok(ref block))
        Some(Err(e)) => return Err(e),
        None => {}
    }
}
```

### reader.rs

```rust
/// Block-level pipelined iteration. Like `for_each_pipelined` but delivers
/// entire `PrimitiveBlock`s (owned) instead of individual elements.
///
/// The consumer receives blocks in file order and can send them to other
/// threads for parallel processing. This enables overlapped I/O + decode +
/// consumer parallelism without blocking the pipeline.
pub fn for_each_block_pipelined<F>(self, f: F) -> Result<()>
where
    F: FnMut(PrimitiveBlock) -> Result<()>,
{
    super::pipeline::run_pipeline(self.blob_iter, f)
}

/// Existing element-level API — now wraps for_each_block_pipelined.
pub fn for_each_pipelined<F>(self, mut f: F) -> Result<()>
where
    F: for<'a> FnMut(Element<'a>),
{
    let is_sorted = self.header.is_sorted();
    let mut last_node_id: i64 = i64::MIN;

    self.for_each_block_pipelined(|block| {
        block.for_each_element(|element| {
            if is_sorted {
                if let Some(id) = node_id(&element) {
                    debug_assert!(
                        id > last_node_id,
                        "Sort.Type_then_ID violated: node {id} <= previous {last_node_id}"
                    );
                    last_node_id = id;
                }
            }
            f(element);
        });
        Ok(())
    })
}
```

### Sorted PBF block classification

Elivagar needs to know whether a block contains nodes, ways, or relations to
route it to the right processing path. In a sorted PBF (Sort.Type_then_ID),
blocks are single-type. PrimitiveBlock already exposes `elements()` but not a
quick type check.

Could add to PrimitiveBlock:
```rust
pub fn block_type(&self) -> Option<BlockType> // DenseNodes | Ways | Relations | Mixed
```

Or the consumer can check the first element. Low priority — elivagar can
implement this themselves.

## Open questions

1. ~~**Should run_pipeline always pass owned blocks?**~~ **Yes — done.** Changed
   `run_pipeline` to pass owned blocks. `for_each_pipelined` is now a wrapper.
   No cost — block was dropped at end of callback anyway.

2. **Should we expose PrimitiveBlock's block type?** Quick classification (nodes
   vs ways vs relations) avoids iterating to the first element. Useful but not
   blocking — can add later.

3. **Should the sorted monotonicity assertion move into PrimitiveBlock?** Currently
   it's in for_each_pipelined. If consumers use for_each_block_pipelined, they
   bypass the assertion. We could add `PrimitiveBlock::assert_sorted()` or put
   it in run_pipeline itself (where blocks are delivered in order). But the
   assertion is debug-only and consumer-facing — keeping it in for_each_pipelined
   seems right.

4. **Channel sizing for the consumer's double-buffer:** The bounded(2) channel in
   the elivagar example means at most 2 way blocks in flight. Each block is ~1-2MB
   decompressed. With larger batches (8K ways = ~1 block), 2 in flight ≈ 4MB.
   Negligible. Could increase to bounded(4) for smoother throughput at the cost
   of ~8MB.
