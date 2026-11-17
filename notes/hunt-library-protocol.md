I found five defects from source inspection. No files were changed, and no builds or tests were run.

1. **[P1] Omitting stream coverage disables stream enforcement.**  
   [run.rs:284](/home/folk/Programs/pbfhogg/crates/library/src/run.rs:284) computes every call's stream verdict, but retains it only when the call declares a nonempty `stream` capability list. Otherwise, even a proven failure is discarded.

   A declaration with omitted `stream` can therefore pass verification despite a missing terminal record, duplicate terminal records, or sealing values that disagree with its snapshot. Other calls can still cover `stream.sealed.v1`, allowing certification to pass too.

   This breaks the [contract that stream accounting must pass](/home/folk/Programs/pbfhogg/reference/library-protocol.md:441). Coverage determines which capabilities a result supports; it should not determine whether protocol violations fail the run. The driver is wrong here.

2. **[P1] Three certifications claim real descriptor-limit behavior that their scenarios do not exercise.**  
   [requirements.rs:12](/home/folk/Programs/pbfhogg/crates/library/src/requirements.rs:12) defines the `nofile.*.v1` capabilities in terms of the soft process limit, including external altw running with soft nofile 1024.

   The worker instead [supplies fixed FdPool grants](/home/folk/Programs/pbfhogg/crates/harness/src/scenarios/nofile.rs:25) of 1024, 6 and 896 descriptors, and [raises the actual soft limit to the hard limit](/home/folk/Programs/pbfhogg/crates/harness/src/process.rs:69). Those scenarios can pass without exercising real-limit probing or the stated limit.

   The grant scenarios are internally consistent. The requirement meanings and their coverage declarations overstate what they certify. Restore scenarios that prove those meanings, or introduce new capability versions for grant-based admission and sizing. Rewording the existing IDs would violate the protocol's meaning-versioning rule.

3. **[P2] Clock validation contradicts the explicitly relative clock contract.**  
   [run.rs:676](/home/folk/Programs/pbfhogg/crates/library/src/run.rs:676) compares absolute `entry_us`, `return_us`, `seal_us` and diagnostic timestamps with process wall time plus tolerance. [FIFO validation](/home/folk/Programs/pbfhogg/crates/library/src/run.rs:237) makes the same absolute comparison.

   The [contract explicitly requires elapsed differences](/home/folk/Programs/pbfhogg/reference/library-protocol.md:271), because the FIFO clock's origin is unreliable. For example, a short call whose readings begin at 10,000,000 us is rejected even when every duration and stream boundary is valid.

   The driver is wrong here. Check call and diagnostic durations relative to `entry_us`, and retain stream checks against the call's recorded interval.

4. **[P2] The worker accepts malformed JSON Unicode escapes.**  
   [json.rs:208](/home/folk/Programs/pbfhogg/crates/harness/src/json.rs:208) parses the four bytes following `\u` using `u32::from_str_radix`. That function accepts a leading `+`; JSON requires four hexadecimal digits.

   Consequently, `"scenario":"repeat-c\u+061t"` is accepted and decoded as `repeat-cat`. With otherwise valid bindings, malformed JSON can reach workload execution.

   This breaks the protocol's strict JSON boundary. Validate all four bytes as hexadecimal digits. More broadly, replacing the handwritten lexer with strict deserialization would remove this source of disagreement between worker and driver.

5. **[P3] Worker request refusals omit promised document paths.**  
   The [contract promises a document path for schema and integer refusals](/home/folk/Programs/pbfhogg/reference/library-protocol.md:60). The worker instead reports duplicate keys and fractional numbers using only [byte offsets](/home/folk/Programs/pbfhogg/crates/harness/src/json.rs:63). A missing required field produces only [“the request lacks a required key”](/home/folk/Programs/pbfhogg/crates/harness/src/request.rs:36).

   These inputs are refused, but the diagnostic contract is broken: callers cannot identify the offending field as promised.

Two additional maintenance/performance observations:

- [Run allocation](/home/folk/Programs/pbfhogg/crates/library/src/run.rs:31) searches permanently reserved IDs from zero on every allocation. Across N runs, allocation performs O(N²) failed create attempts.
- The worker's string parser repeatedly validates the entire remaining input as UTF-8 while consuming one character. Long request strings therefore incur quadratic parsing work.
