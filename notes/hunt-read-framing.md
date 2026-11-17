Static review found three parsing defects, plus a recovery inconsistency and documentation gaps. No files were changed, and no builds or tests were run.

1. **[P1] Blob parsers ignore the wire type of recognized fields.**

   [WireBlob::parse](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob_wire.rs:328) and [parse_raw_envelope](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:572) dispatch on field number alone. Payload fields are always read as length-delimited; `raw_size` is always read as a varint, regardless of the tag.

   A concrete trigger is an otherwise ordinary raw Blob whose payload tag changes from `0x0a` to `0x0e`. That declares reserved wire type 6, but both parsers still consume the following length and payload successfully. If the payload is a valid PrimitiveBlock, semantic decoding succeeds.

   This breaks the claimed protobuf decoding and input-refusal contracts. With other wrong wire types, the parsers can also consume bytes across actual field boundaries instead of skipping the unknown field as protobuf requires. The framing layer cannot catch this because its declared byte extent remains correct.

   The same pattern appears beyond the immediate scope in [WireHeaderBlock::parse](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:137).

2. **[P1] Unknown required features are accepted, consumed, and potentially erased.**

   Header decoding retains arbitrary required-feature strings, but [read_header_blob](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1350) returns the header without checking support. No later semantic reader performs that check.

   Give a normally framed file an additional required feature such as `UnsupportedRequiredFeature`. ElementReader proceeds to decode its elements. A rewriting command can then build a fresh header through [output_header::build](/home/folk/Programs/pbfhogg/crates/lib/src/commands/output_header.rs:280), retaining the recognized history declaration while dropping the unsupported requirement.

   This breaks the meaning explicitly promised by [HeaderBlock::required_features](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:236): these are features the parser *needs to implement* to interpret subsequent blocks. The [vendored libosmium reader](/home/folk/Programs/pbfhogg/research/libosmium/include/osmium/io/detail/pbf_decoder.hpp:1040) refuses unknown required features.

   Retaining the strings for inspection is appropriate. The wrong boundary is admission to semantic consumption without checking them.

3. **[P2] Missing required `BlobHeader.datasize` is silently interpreted as zero.**

   [WireBlobHeader::parse](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob_wire.rs:93) initializes `datasize` to zero and never records whether field 3 occurred. Its final required-field check covers only `type`. [Frames](/home/folk/Programs/pbfhogg/crates/lib/src/read/frame.rs:637) subsequently accepts that manufactured zero.

   Consequently, a header containing a type but no datasize becomes a completed frame with an empty body. Header walks and raw walks accept it. For example, after a valid leading header, an extension frame with no datasize can be silently skipped and the input accepted.

   This breaks the required-field contract of the BlobHeader protobuf. It is independent of payload validation: the frame descriptor itself is incomplete. The parser needs to distinguish an absent size from an explicitly encoded zero. Libosmium rejects both missing and zero sizes; the repository currently explicitly tests acceptance of zero, so that separate compatibility difference also needs an explicit ruling.

The partial-failure review found another concrete inconsistency: **[BlobReader::seek](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:1070) does not resume a failed reader**, whereas `seek_raw` and `blob_from_offset` do. After a malformed frame produces `Err`, successfully seeking to a valid frame still leaves `next()` and `next_header_skip_blob()` returning `None`. The source moved, but the sticky failure remains. This contradicts the internal statement that iteration stops “[until a seek resumes it](/home/folk/Programs/pbfhogg/crates/lib/src/read/blob.rs:838).” The public documentation should either explicitly distinguish this behavior or the public seek operations should share recovery semantics.

Other observations:

- **The deviation registry is incomplete.** The deliberate multi-payload Blob refusal is acknowledged as a compatibility departure in [truncation-handling.md](/home/folk/Programs/pbfhogg/reference/truncation-handling.md:542), but is absent from `DEVIATIONS.md`. That breaks the stated requirement to record deliberate protobuf and osmium departures there.
- **The DenseInfo documentation is stale.** [truncation-handling.md](/home/folk/Programs/pbfhogg/reference/truncation-handling.md:289) says partial column sets are rejected. The [validator](/home/folk/Programs/pbfhogg/crates/lib/src/read/block.rs:1629) deliberately accepts them. The code is correct here. Likewise, [claims.rs](/home/folk/Programs/pbfhogg/crates/lib/src/read/claims.rs:43) says negative versions yield no claims, while the current implementation preserves available non-padding metadata.
- **`raw_blob_payload` overstates its validation.** Its [documentation](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:538) promises matching refusals with `decompress_blob_raw`, but it discards `raw_size` and accepts envelopes the decompressor rejects for size mismatch. Diff's documented byte-equality shortcut explains the behavior; the helper's promise is wrong.
- **There is an avoidable allocation boundary.** [decompress_blob_data_into](/home/folk/Programs/pbfhogg/crates/lib/src/read/decompress.rs:441) copies the entire serialized Blob through `parse_slice`, despite already receiving borrowed bytes and caller-owned output storage.

The strongest structural opportunity is one borrowed Blob-envelope parser shared by owned Blob construction, direct decompression, and payload comparison. It should distinguish structural validation from post-decompression validation explicitly. That would remove duplicated parsing policy and the unnecessary whole-envelope copy.
