#!/usr/bin/env python3
"""Legacy glob helper for rewrite_citations.py's layout migration.

Representative textlint coverage uses brokkr's must_scan sentinels. The former
record/check manifest commands are retired; selector validation belongs to
brokkr, whose check --audit reports dead globs and exclusions. Keep this
helper until the citation migration tooling is retired with its importer.
"""
import re


def glob_regex(glob):
    out, i = [], 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    return re.compile("".join(out) + r"\Z")


if __name__ == "__main__":
    raise SystemExit("coverage manifests are retired; use brokkr must_scan and check --audit")
