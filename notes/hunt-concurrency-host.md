Found four defects by source inspection. Two cross from the scoped infrastructure into the reader session adapter.

1. **[P2] A diagnostic sink panic can become a process abort.** [host.rs:2458](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:2458)

   `SummaryWriter::write` calls the sink before removing the completed line from its buffer. If the sink panics, unwinding drops the writer, whose destructor sends the still-buffered text to that sink again. A sink that panics consistently therefore causes a second panic during unwinding and aborts the process.

   This breaks the repository's stated avoidance of panicking cleanup and the writer's one-event-per-line behavior. `DiagnosticSink` imposes no non-panicking requirement. Remove pending text from the writer's state before calling out, and make destructor delivery safe during unwinding.

2. **[P2] The block iterator returns queued successes after cancellation.** [reader.rs:1393](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1393)

   `PipelinedBlocks::next` admits a call through `Invocation::call`, but never checks its cancellation token. Admission refuses sealing, not cancellation.

   Let the background pipeline fill the iterator's queue and block sending another block, then cancel the invocation. Subsequent `next()` calls still return queued `Ok(block)` values. This occurs while the session remains active, so it does not depend on cancellation arriving after completion.

   It contradicts [the reader's explicit next-blob cancellation promise](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:99). The wrong end is the session adapter: it needs cancellation checkpoints at its receive/delivery boundary.

3. **[P2] Reader teardown defeats the infrastructure's panic-payload protection.** [reader.rs:1442](/home/folk/Programs/pbfhogg/crates/lib/src/read/reader.rs:1442)

   `threads::scope` preserves a primary panic and forgets secondary payloads; `JoinedPool` likewise forgets caught payloads because their destructors may panic. But `PipelinedBlocks::drop` uses `drop(h.join())`, which destroys an error payload.

   A caller-supplied `Read` can panic after the header with a payload whose destructor also panics. If the iterator is subsequently dropped during another unwind, destroying that payload aborts the process. The disconnect branch of `next()` also destroys the payload after formatting it, potentially panicking instead of returning its promised internal error.

   This breaks the teardown's own stated intent to suppress the background panic during cleanup. The session boundary should consume caught payloads using the same policy as the scoped infrastructure.

4. **[P3] `SummaryWriter::flush` succeeds without flushing buffered output.** [host.rs:2470](/home/folk/Programs/pbfhogg/crates/lib/src/host.rs:2470)

   Writing an unterminated line and calling `flush()` returns success while delivering nothing to the diagnostic sink. The bytes remain buffered until a newline or destruction.

   That violates the implemented `std::io::Write::flush` contract. Flushing should deliver the pending fragment, as destruction already does.

Two additional observations:

- **Incorrect documentation:** [threads.rs:43](/home/folk/Programs/pbfhogg/crates/lib/src/threads.rs:43) promises that an unjoined result is destroyed on its worker thread. The result actually lives in an `Arc<Mutex<Option<T>>>`. If the worker finishes before the caller drops its handle, the caller destroys it. The documentation is wrong; the implementation provides no destruction-thread guarantee.
- **Unmeasured optimization opportunity:** `SummaryWriter::write` repeatedly drains the front of a `Vec`. One large write containing many short lines repeatedly shifts the remaining bytes, giving quadratic copying. Walking completed lines once and retaining only the unfinished suffix would remove that shape.

No files were changed, and no builds or tests were run.
