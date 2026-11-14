//! `decode-*` and `encode-*`: the synthetic read and write workloads
//! (`crate::synthetic`), one scenario per arm so each is a closed plan.
//! Each is one call on the indexed dataset, the process's only library
//! work, so profilable. The assertion reads the call's record: ok, and
//! counts returned. What the counts and the encoded bytes are is the unit
//! tests' to prove on synthetic input; on a dataset there is no oracle,
//! and an empty input is a valid one.

use super::{Scenario, account, assertion};
use crate::protocol::Outcome;
use crate::runner::{CallRecord, Done, Observation};
use crate::synthetic::{Reader, Writer};
use crate::workload::{CallPlan, Command, HostPlan, InputRole, Returned, Workload};
use pbfhogg::host::PageCachePolicy;
use pbfhogg::writer::Compression;

/// The decode arms: id, label, reader.
const DECODE: &[(&str, &str, Reader)] = &[
    (
        "decode-sequential",
        "count input through ElementReader::for_each",
        Reader::Sequential,
    ),
    (
        "decode-parallel",
        "count input through ElementReader::par_map_reduce",
        Reader::Parallel,
    ),
    (
        "decode-pipelined",
        "count input through ElementReader::for_each_pipelined",
        Reader::Pipelined,
    ),
    (
        "decode-blobreader",
        "count input through BlobReader, decoding each blob in turn",
        Reader::Blobreader,
    ),
];

/// The encode arms: id, label, writer, compression.
const ENCODE: &[(&str, &str, Writer, Compression)] = &[
    (
        "encode-sync-none",
        "encode input into the discard sink, sync writer, uncompressed",
        Writer::Sync,
        Compression::None,
    ),
    (
        "encode-sync-zlib-6",
        "encode input into the discard sink, sync writer, zlib level 6",
        Writer::Sync,
        Compression::Zlib(6),
    ),
    (
        "encode-sync-zstd-3",
        "encode input into the discard sink, sync writer, zstd level 3",
        Writer::Sync,
        Compression::Zstd(3),
    ),
    (
        "encode-pipelined-none",
        "encode input into the discard sink, pipelined writer, uncompressed",
        Writer::Pipelined,
        Compression::None,
    ),
    (
        "encode-pipelined-zlib-6",
        "encode input into the discard sink, pipelined writer, zlib level 6",
        Writer::Pipelined,
        Compression::Zlib(6),
    ),
    (
        "encode-pipelined-zstd-3",
        "encode input into the discard sink, pipelined writer, zstd level 3",
        Writer::Pipelined,
        Compression::Zstd(3),
    ),
];

/// What one arm runs.
#[derive(Clone, Copy)]
enum Arm {
    Decode(Reader),
    Encode(Writer, Compression),
}

/// One synthetic arm.
pub(crate) struct Synthetic {
    id: &'static str,
    label: &'static str,
    arm: Arm,
}

/// Every arm, decode first, in table order.
pub(crate) fn all() -> Vec<Synthetic> {
    let decode = DECODE.iter().map(|&(id, label, reader)| Synthetic {
        id,
        label,
        arm: Arm::Decode(reader),
    });
    let encode = ENCODE
        .iter()
        .map(|&(id, label, writer, compression)| Synthetic {
            id,
            label,
            arm: Arm::Encode(writer, compression),
        });
    decode.chain(encode).collect()
}

impl Scenario for Synthetic {
    fn workload(&self) -> Workload {
        let mut w = Workload::new(
            self.id,
            "one synthetic read or write workload on the indexed input: profilable",
            3600,
        );
        w.hotpath = true;
        w.inputs = vec![InputRole::dataset("input", "indexed")];
        let (command, page_cache) = match self.arm {
            // Evict behind the read head, as the read benchmark always has:
            // the workload reads the whole file once. Not a cold-cache claim.
            Arm::Decode(reader) => (
                Command::Decode {
                    input: "input",
                    reader,
                },
                PageCachePolicy::Evict,
            ),
            Arm::Encode(writer, compression) => (
                Command::Encode {
                    input: "input",
                    writer,
                    compression,
                },
                PageCachePolicy::Keep,
            ),
        };
        let mut call = CallPlan::plain("a", self.label, command);
        call.host = HostPlan {
            page_cache,
            ..HostPlan::PLAIN
        };
        w.calls = vec![call];
        w.assertions = vec![assertion("a.outcome", Some("a"), &[])];
        w
    }

    fn observe(&self) -> Box<dyn Observation> {
        Box::new(Judge)
    }
}

struct Judge;

impl Observation for Judge {
    fn judge_call(&self, record: &CallRecord, _done: &Done<'_>) -> Vec<Outcome> {
        let counts = match record.returned {
            Some(Returned::Counts(counts)) => Some(counts),
            _ => None,
        };
        let ok = record.outcome.is_ok()
            && record.cancel.is_none()
            && record.publications.is_empty()
            && counts.is_some();
        vec![Outcome::judge(
            "a.outcome",
            ok,
            format!("{}, counts {counts:?}", account(record)),
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::{DECODE, ENCODE, all};
    use crate::scenarios::Scenario;
    use crate::workload::Command;

    /// Each arm's id, plan and command come from the same table row.
    #[test]
    fn every_arm_runs_its_own_row() {
        let arms = all();
        assert_eq!(arms.len(), DECODE.len() + ENCODE.len());
        for (arm, &(id, _, reader)) in arms.iter().zip(DECODE) {
            let w = arm.workload();
            assert_eq!(w.id, id);
            assert!(matches!(w.calls[0].command, Command::Decode { reader: r, .. } if r == reader));
        }
        for (arm, &(id, _, writer, compression)) in arms[DECODE.len()..].iter().zip(ENCODE) {
            let w = arm.workload();
            assert_eq!(w.id, id);
            assert!(matches!(
                w.calls[0].command,
                Command::Encode { writer: wr, compression: c, .. } if wr == writer && c == compression
            ));
        }
        for arm in &arms {
            let w = arm.workload();
            w.validate().expect("valid workload");
            crate::scenarios::declared(&w)
                .validate()
                .expect("valid declaration");
        }
    }
}
