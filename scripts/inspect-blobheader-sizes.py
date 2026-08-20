#!/usr/bin/env python3
"""Independent hand-decoder for BlobHeader size and tagdata occupancy.

Deliberately does not import pbfhogg: the point is to read what actually
landed on the wire with a decoder that shares no code with the writer.

The BlobHeader is capped at 65536 bytes by the format, and tagdata (field 4)
is the only header field whose size scales with blob contents. This walks the
frames header-only (no decompression, no element decode) and reports the
distribution so "does --elements-per-blob N overflow the header?" is answered
by measurement rather than by reasoning about tag density.

Usage: inspect-blobheader-sizes.py <file.osm.pbf> [max_blobs]

max_blobs defaults to 0 meaning "all blobs".
"""

import struct
import sys


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
    tagdata = None
    while i < len(h):
        key, i = read_varint(h, i)
        field, wire = key >> 3, key & 7
        if wire == 2:
            ln, i = read_varint(h, i)
            val = h[i:i + ln]
            i += ln
            if field == 1:
                blob_type = val
            elif field == 4:
                tagdata = val
        elif wire == 0:
            v, i = read_varint(h, i)
            if field == 3:
                datasize = v
        else:
            raise SystemExit(f"unexpected wire type {wire}")
    return blob_type, datasize, tagdata


def tag_key_count(tagdata):
    if tagdata is None or len(tagdata) < 3:
        return None
    if tagdata[0] != 0x01:
        return None
    return struct.unpack("<H", tagdata[1:3])[0]


def main():
    if len(sys.argv) < 2:
        raise SystemExit(__doc__.strip())
    path = sys.argv[1]
    limit = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    max_header = 0
    max_tagdata = 0
    max_keys = 0
    no_tagdata = 0
    seen = 0
    with open(path, "rb") as f:
        while limit == 0 or seen < limit:
            prefix = f.read(4)
            if len(prefix) < 4:
                break
            (n,) = struct.unpack(">I", prefix)
            header = f.read(n)
            blob_type, datasize, tagdata = parse_blob_header(header)
            f.read(datasize)
            if blob_type != b"OSMData":
                continue
            seen += 1
            max_header = max(max_header, n)
            if tagdata is None:
                no_tagdata += 1
            else:
                max_tagdata = max(max_tagdata, len(tagdata))
                keys = tag_key_count(tagdata)
                if keys is not None:
                    max_keys = max(max_keys, keys)
    print(f"OSMData blobs:      {seen}")
    print(f"max BlobHeader:     {max_header} bytes (cap 65536)")
    print(f"max tagdata:        {max_tagdata} bytes")
    print(f"max tagdata keys:   {max_keys}")
    print(f"blobs w/o tagdata:  {no_tagdata}")


if __name__ == "__main__":
    main()
