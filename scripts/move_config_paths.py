#!/usr/bin/env python3
"""One-off for the crates/ move: rewrite old-layout path tokens in the
configuration and script files named on the command line through
scripts/move_map.py.

    move_config_paths.py FILE...

A token is a moved prefix (src/, tests/, examples/, cli/, harness/) not
preceded by a path or word character, so docs/cli/commands.md and
crates/lib/src/ are left alone. Prints each rewrite.
"""
from pathlib import Path
import re
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from move_map import MOVE, moved  # noqa: E402

PREFIXES = "|".join(re.escape(old.rstrip("/")) for old, _ in MOVE)
TOKEN = re.compile(rf"(?<![\w./~@:-])((?:{PREFIXES})/[\w./*-]*)")


def main(argv):
    for name in argv:
        path = Path(name)
        lines = path.read_text().split("\n")
        for n, line in enumerate(lines):
            new = TOKEN.sub(lambda m: moved(m.group(1)) or m.group(1), line)
            if new != line:
                print(f"{name}:{n + 1}: {line.strip()}\n  -> {new.strip()}")
                lines[n] = new
        path.write_text("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
