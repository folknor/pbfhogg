#!/usr/bin/env python3
"""Conservatively enforce one source-level test per static-hook target.

Count even disabled attributes, so cfg changes cannot silently add a sibling.
Out-of-line modules and includes need review rather than hiding extra tests.
Also checks that every pbfhogg-cli test harness refuses, at compile time, to
link a pbfhogg built with the static hooks.
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
# pbfhogg-cli's harnesses link the library but cannot see its features as a
# cfg, so each must refuse at compile time to link a build with the statics.
# Every harness it has - the lib's unit tests and each [[test]] target - must
# carry the assertion.
cli = root / "cli"
cli_manifest = tomllib.loads((cli / "Cargo.toml").read_text())


def active_assertion(source, allowed_cfg):
    """An item-level `const _: () = assert!(!pbfhogg::STATIC_TEST_HOOKS, ...)`
    at the start of a line, outside any comment, preceded by no attribute or
    only `allowed_cfg`: commented-out, cfg'd-away or function-local spellings
    do not count."""
    item = re.compile(
        r"^const\s+_\s*:\s*\(\s*\)\s*=\s*assert!\s*\(\s*!\s*pbfhogg::STATIC_TEST_HOOKS\b",
        re.M,
    )
    for m in item.finditer(source):
        # Nearest preceding line that is neither blank nor a line comment.
        lines = source[: m.start()].split("\n")
        before = ""
        for line in reversed(lines):
            line = line.strip()
            if line and not line.startswith("//"):
                before = line
                break
        if not before.startswith("#[") or before == allowed_cfg:
            return True
    return False


# The lib's unit-test harness: its assertion is the one `#[cfg(test)]` item.
lib = cli_manifest.get("lib", {}).get("path", "src/lib.rs")
if not active_assertion((cli / lib).read_text(), "#[cfg(test)]"):
    errors.append(f"cli/{lib}: the lib's unit-test harness must assert !pbfhogg::STATIC_TEST_HOOKS")
# Every integration-test harness, unconditionally.
for target in cli_manifest.get("test", []):
    path = target.get("path", f"tests/{target['name']}.rs")
    if not active_assertion((cli / path).read_text(), None):
        errors.append(f"cli/{path}: a pbfhogg-cli test harness must assert !pbfhogg::STATIC_TEST_HOOKS")
# A bin's unit-test harness links the CLI lib built without cfg(test), so the
# lib's assertion is absent there: bins must not have one.
for target in cli_manifest.get("bin", []):
    if target.get("test", True):
        errors.append(f"cli/Cargo.toml: bin {target['name']} must set test = false")
if "static-test-hooks" in cli_manifest.get("features", {}):
    errors.append("cli/Cargo.toml: pbfhogg-cli must not declare a static-test-hooks feature")
if errors:
    print("\n".join(errors), file=sys.stderr)
    sys.exit(1)
print("static hook targets ok")
