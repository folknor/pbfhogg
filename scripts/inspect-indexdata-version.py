#!/usr/bin/env python3
"""Independent hand-decoder for BlobHeader.indexdata version + bbox words.

Deliberately does not import pbfhogg: the point is to read what actually
landed on the wire with a decoder that shares no code with the writer.

Answers "what indexdata version did this file actually get written with, and
how does it spell an absent bbox" for a real PBF of any size, without decoding
a single element. That question is not answerable from a test fixture: the
first run of `check --index` against the real 464 MB Denmark extract reported
all 6562 of its node blobs as lying, and the reason - a v1 index has no bbox
field at all - was found with this decoder.

Usage: inspect-indexdata-version.py <file.osm.pbf> [max_blobs]

max_blobs defaults to 20; the walk is seek-free and header-only, so raising it
to cover a planet-scale file costs one pass over the frames, not a decode.
"""

import struct
import sys
from collections import Counter


def read_varint(buf, i):
    v = 0
    s = 0
    while True:
        b = buf[i]
        i += 1
        v |= (b & 0x7F) << s
        s += 7
        if not b & 0x80:
            return v, i


def parse_blob_header(h):
    i = 0
    blob_type = None
    datasize = None
    indexdata = None
    while i < len(h):
        key, i = read_varint(h, i)
        field, wire = key >> 3, key & 7
        if wire == 2:
            ln, i = read_varint(h, i)
            val = h[i:i + ln]
            i += ln
            if field == 1:
                blob_type = val
            elif field == 2:
                indexdata = val
        elif wire == 0:
            v, i = read_varint(h, i)
            if field == 3:
                datasize = v
        else:
            raise SystemExit(f"unexpected wire type {wire}")
    return blob_type, datasize, indexdata


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__.strip())
    path = sys.argv[1]
    limit = int(sys.argv[2]) if len(sys.argv) > 2 else 20
    versions = Counter()
    absent_bbox = Counter()
    with open(path, "rb") as f:
        seen = 0
        while seen < limit:
            prefix = f.read(4)
            if len(prefix) < 4:
                break
            (n,) = struct.unpack(">I", prefix)
            header = f.read(n)
            blob_type, datasize, indexdata = parse_blob_header(header)
            f.read(datasize)
            if blob_type != b"OSMData":
                continue
            seen += 1
            if indexdata is None:
                versions["absent"] += 1
                continue
            versions[hex(indexdata[0])] += 1
            if len(indexdata) >= 42:
                words = struct.unpack("<4i", indexdata[26:42])
                inverted = words[0] > words[1] or words[2] > words[3]
                absent_bbox["inverted (v3 absent)" if inverted
                            else "zeros" if words == (0, 0, 0, 0)
                            else "real box"] += 1
    print("indexdata versions:", dict(versions))
    print("bbox words:", dict(absent_bbox))


if __name__ == "__main__":
    main()
