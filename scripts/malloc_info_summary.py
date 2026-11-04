#!/usr/bin/env python3
"""Summarise a glibc malloc_info(3) XML dump: one line per arena.

Usage: malloc_info_summary.py DUMP.xml [DUMP.xml ...]

Per heap: arena number, bytes the arena holds from the system ("current"),
free bytes in it (fastbins plus the rest), so held minus free is live.
"""
import sys
import xml.etree.ElementTree as ET

MB = 1024 * 1024


def summarise(path):
    root = ET.parse(path).getroot()
    rows = []
    for heap in root.findall("heap"):
        nr = int(heap.get("nr"))
        current = 0
        free = 0
        for system in heap.findall("system"):
            if system.get("type") == "current":
                current = int(system.get("size"))
        for total in heap.findall("total"):
            if total.get("type") in ("fast", "rest"):
                free += int(total.get("size"))
        rows.append((nr, current, free))
    print(f"== {path}: {len(rows)} arenas")
    held = sum(r[1] for r in rows)
    free = sum(r[2] for r in rows)
    print(f"   held {held / MB:9.1f} MB  free {free / MB:9.1f} MB  live {(held - free) / MB:9.1f} MB")
    for nr, current, fr in rows:
        print(f"   arena {nr:3d}  held {current / MB:8.2f} MB  free {fr / MB:8.2f} MB")


for p in sys.argv[1:]:
    summarise(p)
