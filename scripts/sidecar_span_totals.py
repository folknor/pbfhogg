"""Sum per-phase sidecar figures over named START/END spans, per brokkr cell.

The default `brokkr sidecar <uuid>` view segments a run between consecutive
markers, so a span holding many inner markers (WAIT_* pairs, schedule
walks) is split into many rows. This adds up every segment that starts
inside each span: CPU (user and kernel core-seconds, from avg cores times
segment length), disk read and write, and major faults.

Usage: python3 scripts/sidecar_span_totals.py SPAN[,SPAN...] UUID [UUID ...]
"""
import json
import subprocess
import sys


def sidecar(uuid, *args):
    out = subprocess.run(
        ["brokkr", "sidecar", uuid, *args], capture_output=True, text=True, check=True
    ).stdout
    return [json.loads(line) for line in out.splitlines() if line.startswith("{")]


def main():
    spans = sys.argv[1].split(",")
    print("cell\tspan\twall_s\tuser_cs\tkernel_cs\tread_GB\twrite_GB\tmajflt")
    for uuid in sys.argv[2:]:
        windows = {
            r["name"]: (r["start_us"], r["end_us"])
            for r in sidecar(uuid, "--durations")
            if r.get("type") == "phase" and r["name"] in spans
        }
        segments = [r for r in sidecar(uuid) if r.get("type") == "phase"]
        for span in spans:
            if span not in windows:
                print(f"{uuid}\t{span}\t-")
                continue
            start, end = windows[span]
            user = kernel = read = write = majflt = 0.0
            for seg in segments:
                if not start <= seg["start_us"] < end:
                    continue
                secs = seg.get("sample_span_us", 0) / 1e6
                user += (seg.get("user_cores") or 0) * secs
                kernel += (seg.get("kernel_cores") or 0) * secs
                read += seg.get("disk_read_kb", 0) or 0
                write += seg.get("disk_write_kb", 0) or 0
                majflt += seg.get("majflt_delta", 0) or 0
            print(
                f"{uuid}\t{span}\t{(end - start) / 1e6:.1f}\t{user:.1f}\t{kernel:.1f}"
                f"\t{read / 1e6:.2f}\t{write / 1e6:.2f}\t{majflt:.0f}"
            )


main()
