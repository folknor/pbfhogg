#!/usr/bin/env python3
"""Conservatively enforce one source-level test per static-hook target.

Count even disabled attributes, so cfg changes cannot silently add a sibling.
Out-of-line modules and includes need review rather than hiding extra tests.
"""
from pathlib import Path
import re
import sys
import tomllib

root = Path(__file__).resolve().parent.parent
manifest = tomllib.loads((root / "Cargo.toml").read_text())
errors = []
for target in manifest.get("test", []):
    if "static-test-hooks" not in target.get("required-features", []):
        continue
    path = target.get("path", f"tests/{target['name']}.rs")
    source = (root / path).read_text()
    # Shared fixture helpers are permitted only while they define no tests.
    source = re.sub(r"\bmod\s+common\s*;", "", source)
    count = len(re.findall(r"#\s*\[\s*test\s*\]", source))
    if count != 1:
        errors.append(f"{path}: expected exactly one #[test], found {count}")
    if re.search(r"\bmod\s+\w+\s*;|\binclude\s*!|\bcfg_attr\s*\([^\]]*\btest\b|\bmacro_rules\s*!", source):
        errors.append(f"{path}: indirect test definitions require an isolated target")
for helper in (root / "tests/common").rglob("*.rs"):
    helper_source = helper.read_text()
    # Fixture self-tests live in a final module, excluded from fault binaries.
    helper_source = re.sub(
        r'#\[cfg\(all\(test, not\(feature = "static-test-hooks"\)\)\)\]'
        r'\s*mod\s+\w+\s*\{[\s\S]*\}\s*\Z',
        "",
        helper_source,
    )
    if re.search(r"#\s*\[\s*(?:test|cfg_attr\s*\([^\]]*\btest\b)", helper_source):
        errors.append(f"{helper.relative_to(root)}: shared fault helpers must contain no tests")
if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print("static hook targets ok")
