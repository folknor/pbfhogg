//! Independent, driver-owned certification requirements.
use super::certify::Requirements;

pub(crate) fn applied() -> Requirements {
    Requirements::new([
        ("calls.repeatable.v1", "two calls in one process, one after another, each install their output and the outputs are byte-identical"),
        ("calls.concurrent.v1", "two calls in one process with intersecting lifetimes both complete and install their outputs"),
        ("counters.own-call.v1", "under concurrency each call's instrument sink receives exactly its own cat counters, once each, with its own values"),
        ("counters.own-snapshot.v1", "concurrent calls' invocation counter snapshots hold only their own values"),
        ("stream.sealed.v1", "a sealed call's stream carries every snapshot counter and one terminal record, with no proven defect"),
        ("fd.pool.v1", "concurrent calls share a caller's descriptor pool, each holding its full grant from before its scan to the end of its scan and returning it whole"),
        ("nofile.cat-refused.v1", "a cat needing more descriptors than the soft limit allows is refused with a typed descriptor error before acquiring any input, publishing nothing"),
        ("nofile.multi-extract-refused.v1", "a single-pass multi-region extract needing more descriptors than the soft limit leaves free is refused with a typed descriptor error before acquiring any input, publishing no member"),
        ("nofile.altw-external.v1", "at caller parallelism 8 and soft nofile 1024, altw external sizes stage 1 to a cpu cap of 6 and a descriptor cap of 3, runs 3 workers, and completes"),
        ("executor.global-pool-unused.v1", "a call on a caller-built Executor::Pool completes without ever building rayon's global pool"),
        ("executor.caller-pool.v1", "writer framing runs on the caller's Executor::Pool"),
        ("host.page-cache-keep.v1", "a call on PageCachePolicy::Keep evicts nothing of its inputs from the page cache"),
        ("cancel.pre-decision.v1", "a cancel before the publication decision ends the call cancelled and publishes nothing"),
        ("cancel.post-decision.v1", "a cancel after the publication decision leaves the call ok and its output installed"),
        ("errors.refused-unpublished.v1", "a call refused for an input that is no PBF reports its typed kind and publishes nothing"),
        ("output.replace.v1", "Overwrite::Replace replaces a seeded destination with the call's output"),
        ("output.refuse-existing.v1", "a call that may not overwrite refuses a seeded destination and leaves the seed intact"),
    ].into_iter().map(|(id, meaning)| (id.to_owned(), meaning.to_owned())).collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    /// Supplied describe document, not harness types or brokkr configuration.
    /// Refresh with brokkr run pbfhogg-harness -- --describe-harness.
    #[test]
    fn declarations_match_the_require_table() {
        let doc = crate::describe::parse(include_bytes!("../fixtures/describe.json")).unwrap();
        let required = super::applied();
        let mut covered = std::collections::BTreeSet::new();
        for scenario in &doc.scenarios {
            for call in &scenario.calls {
                assert_eq!(
                    call.stream,
                    ["stream.sealed.v1"],
                    "{}/{}",
                    scenario.id,
                    call.id
                );
                covered.extend(call.stream.iter().cloned());
            }
            covered.extend(
                scenario
                    .assertions
                    .iter()
                    .flat_map(|a| a.covers.iter().cloned()),
            );
        }
        assert_eq!(required.list().len(), 17);
        assert_eq!(covered, required.list().keys().cloned().collect());
    }
}
