use super::finish_invocation;
use pbfhogg::debug::InstrumentSink;
use pbfhogg::host::Invocation;
use pbfhogg::writer::{Compression, PbfWriter};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl InstrumentSink for Recording {
    fn marker(&self, _name: &str) {}

    fn counter(&self, name: &str, _value: i64) {
        self.0.lock().expect("recording").push(name.to_owned());
    }
}

#[test]
fn dispatch_finalizes_success_refusal_and_cancellation_after_work() {
    for outcome in [Ok(()), Err("refused"), Err("cancelled")] {
        let sink = Arc::new(Recording::default());
        let invocation = Invocation::with_sink(Arc::clone(&sink) as Arc<dyn InstrumentSink>);
        let result = finish_invocation(&invocation, || -> Result<(), &str> {
            assert!(sink.0.lock().expect("recording").is_empty());
            // Writer emission deliberately suppresses an all-zero set.
            // Exercise a real writer so removing that hook loses evidence.
            let mut writer = PbfWriter::new_in(Vec::new(), Compression::None, invocation.clone())
                .expect("writer");
            writer.write_header(&[]).expect("header");
            writer.flush().expect("flush");
            drop(writer);
            assert!(sink.0.lock().expect("recording").is_empty());
            if outcome == Err("cancelled") {
                let _ = invocation.cancel();
            }
            // An early return from dispatch must still reach finalization.
            outcome?;
            Ok(())
        });
        assert_eq!(result, outcome);
        let counters = sink.0.lock().expect("recording");
        for name in [
            "writer_permit_wait_ns",
            "pipeline_decompress_pool_full_drops",
        ] {
            assert_eq!(
                counters
                    .iter()
                    .filter(|value| value.as_str() == name)
                    .count(),
                1,
            );
        }
    }
}
