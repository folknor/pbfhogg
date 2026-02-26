# `block_builder::take` buffer reuse investigation

Investigation of the 4.6 GB allocation churn in `BlockBuilder::take()`, profiled on
Denmark seq4704 (483 MB, 59.1M elements, 7396 blocks). Hotpath data from
`notes/hotpath-profile.md` (commit d5c8095, fat LTO, zlib-ng).

## Allocation breakdown

`take()` allocates 4.6 GB across 7396 calls. Breakdown by source:

### 1. Dense Vec re-allocation in `reset()` — ~3.3 GB (72%)

`take_dense_nodes_group()` (`block_builder.rs:649`) uses `std::mem::take()` to move
all dense accumulator Vecs into the proto message (zero-copy pointer move). This
leaves them at zero capacity. `reset()` (`block_builder.rs:714-725`) then
re-allocates each with `Vec::with_capacity(MAX_ENTITIES_PER_BLOCK)`:

    10 arrays × 8000 capacity × avg ~6 bytes/element ≈ 456 KB per dense-node block
    456 KB × ~7200 dense-node blocks ≈ 3.3 GB

**Not optimizable** with prost's ownership model. The proto message must own the
Vec data for encoding. The alternative — clone Vecs into proto, `clear()` in place
— trades allocation for memcpy at roughly equal or worse cost per cycle (both
approaches do 1 alloc + 1 dealloc, but clone adds a memcpy of ~456 KB ≈ 45 µs).
The current `mem::take` approach is already optimal.

### 2. `encode_to_vec()` — ~960 MB (21%) ← reuse target

Line 632: `let bytes = block.encode_to_vec();`

Each call allocates a fresh `Vec<u8>`, fills it with the serialized PrimitiveBlock,
returns it. The caller passes it to `write_primitive_block(&bytes)` and immediately
drops it. Typical serialized block size: ~130 KB.

    ~130 KB × 7396 blocks ≈ 960 MB

After the first `take()`, all subsequent blocks are roughly the same size (8000
entities). A reused buffer stabilizes at ~130 KB capacity and never re-allocates.

### 3. `StringTable::new()` — ~130 MB (3%)

Line 618: `std::mem::replace(&mut self.string_table, StringTable::new())`

Each new StringTable pre-allocates `Vec::with_capacity(256)` +
`FxHashMap::with_capacity_and_hasher(256, ...)` ≈ 18 KB.

    18 KB × 7396 ≈ 130 MB

Not worth optimizing. `into_proto()` consumes the StringTable (moves String buffers
into `Bytes`), so clear-and-reuse would require cloning strings into the proto
instead of moving them. The String data itself (tag keys/values) must end up owned
by the proto either way.

### 4. Proto defaults + overhead — ~200 MB (4%)

`proto::PrimitiveBlock::default()`, `PrimitiveGroup::default()`, `DenseNodes::default()`,
etc. Small per-call overhead, not actionable.

## The optimization

Store a `Vec<u8>` inside `BlockBuilder`. In `take()`, call `self.encode_buf.clear()`
then `block.encode(&mut self.encode_buf)` instead of `block.encode_to_vec()`.

Prost's `Message::encode()` takes `&mut impl BufMut`. `Vec<u8>` implements `BufMut`
(appends, grows as needed). After the first block, the buffer has sufficient capacity
and all subsequent encodes are zero-allocation.

Note: `encode()` to a `Vec<u8>` cannot fail — Vec grows dynamically, so
`remaining_mut()` is effectively unlimited. The `Result<(), EncodeError>` return is
defensive only.

Pre-sizing the buffer (via `encoded_len()` or a heuristic like 512 KB) is not
worthwhile. `encoded_len()` traverses the entire proto tree, which is wasted work
when the Vec already has capacity. Letting the first encode grow the buffer naturally
costs ~3-5 re-allocs once, then all subsequent calls are free.

## API change: `Vec<u8>` → `&[u8]`

The return type must change from owned to borrowed:

```rust
// Before
pub fn take(&mut self) -> io::Result<Option<Vec<u8>>>

// After
pub fn take(&mut self) -> io::Result<Option<&[u8]>>
```

If `take()` returns `Vec<u8>`, it must give away the buffer (defeating reuse).
Returning `&[u8]` keeps the buffer inside BlockBuilder and lends it to the caller.

### Caller pattern analysis

Every call site (79 direct calls across commands, tests, examples) follows the
same pattern:

```rust
if let Some(bytes) = bb.take()? {
    writer.write_primitive_block(&bytes)?;
}
// bytes dropped, bb is free for add_*/take again
```

The borrow lifetime works: `take(&mut self) -> Option<&[u8]>` borrows from `self`.
The caller holds the `&[u8]` only until `write_primitive_block` returns, then the
borrow is released and `&mut self` methods are available again.

Migration is mechanical: `&bytes` → `bytes` at call sites (or no change where the
value is already used as a slice). Tests that do `.take().unwrap().unwrap()` and
pass to `write_primitive_block` need the same `&` removal.

Not published to crates.io yet, so this is not a semver-breaking concern.

### Pipelined writer interaction

In pipelined mode (`writer.rs:287`), `write_primitive_block` clones the bytes for
the rayon task:

```rust
let uncompressed = block_bytes.to_vec();  // clone for rayon ownership
```

This `.to_vec()` still allocates — rayon tasks need owned data. But the encode
buffer allocation is eliminated. Previously each block had two allocations (encode
+ rayon clone), now it has one (rayon clone only). In sync mode, it goes from one
to zero.

## Implementation

```rust
pub struct BlockBuilder {
    // ... existing fields ...
    encode_buf: Vec<u8>,
}

impl BlockBuilder {
    pub fn new() -> Self {
        BlockBuilder {
            // ... existing fields ...
            encode_buf: Vec::new(),  // grows on first take(), reused thereafter
        }
    }

    pub fn take(&mut self) -> io::Result<Option<&[u8]>> {
        let block_type = match self.block_type {
            Some(t) => t,
            None => return Ok(None),
        };

        let mut block = proto::PrimitiveBlock::default();
        let string_table = std::mem::replace(&mut self.string_table, StringTable::new());
        block.stringtable = string_table.into_proto();

        let group = match block_type {
            BlockType::DenseNodes => self.take_dense_nodes_group(),
            BlockType::Ways => self.take_ways_group(),
            BlockType::Relations => self.take_relations_group(),
        };
        block.primitivegroup.push(group);

        self.encode_buf.clear();
        block
            .encode(&mut self.encode_buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.reset();
        Ok(Some(&self.encode_buf))
    }
}
```

`reset()` does not touch `encode_buf`, so the borrow returned from `take()` remains
valid after reset.

## Expected savings

| Metric | Before | After | Delta |
|--------|--------|-------|-------|
| Allocs per block (encode) | 1 × ~130 KB | 0 (after first) | -7395 allocs |
| Total encode alloc churn | ~960 MB | ~130 KB (first only) | -99.99% |
| Total `take()` alloc churn | 4.6 GB | ~3.6 GB | -21% |
| Wall time (`take()`, est.) | 3.46s | ~3.1–3.3s | -5–10% |

Wall time savings are modest because encoding work dominates allocation overhead.
The primary win is allocation pressure — at planet scale (80× Denmark), this
eliminates ~75 GB of allocator churn, reducing TLB flushes, page faults, and
allocator lock contention.

## Out of scope

These are separate optimizations, noted here for completeness:

- **`frame_blob` encode_to_vec (4.0 GB, `writer.rs:603/615`):** Runs in rayon tasks
  for pipelined mode. Would need per-thread buffers via `thread_local::ThreadLocal`.
- ~~**`add_way` Vec allocation (4.1 GB):**~~ Resolved by direct wire-format
  encoding. Ways/relations now encode directly to protobuf bytes using reusable
  scratch buffers — no `proto::Way`/`proto::Relation` Vec allocations.
- **Dense Vec re-allocation (3.3 GB):** Already optimal, see section 1 above.
