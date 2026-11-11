"""Recording brokkr stub for scripts/vs-osmium.sh; no datasets or scratch files.

brokkr's bench_tests directory runs Lua contract tests only, so this shell
driver test is a pre-clippy script check like the other Python self-tests.
"""
import os
from pathlib import Path
import subprocess
import unittest

ROOT = Path(__file__).resolve().parents[1]


class DriverContract(unittest.TestCase):
    def drive(self, *args):
        env = dict(os.environ)
        env["BASH_FUNC_brokkr%%"] = '() { printf "%s\\n" "$*"; }'
        return subprocess.run(
            ["bash", str(ROOT / "scripts/vs-osmium.sh"), *args],
            cwd=ROOT, env=env, text=True, capture_output=True, check=False,
        )

    def test_rejects_bad_pairs(self):
        for pairs in ("0", "-1", "", "1.5", "x", "+1", "99999999999999999999"):
            with self.subTest(pairs=pairs):
                result = self.drive(pairs, "fixture", "9")
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, "")

    def test_requires_dataset_and_osc(self):
        for args in (("1",), ("1", "fixture"), ("1", "", "9"), ("1", "fixture", "")):
            with self.subTest(args=args):
                result = self.drive(*args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, "")

    def test_explicit_selection_and_alternation(self):
        result = self.drive("2", "fixture", "9")
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 46)  # 11 paired rows plus inspect, twice.
        for line in lines:
            self.assertIn("--dataset fixture", line)
            self.assertIn("--iterations 1", line)
            self.assertEqual("--osc 9" in line, "--row apply-changes " in line)
        extracts = [line for line in lines if "--row extract " in line]
        self.assertEqual(len(extracts), 4)
        self.assertEqual([line.split("--tool ")[1] for line in extracts],
                         ["pbfhogg", "osmium", "osmium", "pbfhogg"])
        inspect = [line for line in lines if "--row inspect " in line]
        self.assertEqual(len(inspect), 2)
        self.assertTrue(all(line.endswith("--tool pbfhogg") for line in inspect))


if __name__ == "__main__":
    result = unittest.TextTestRunner().run(
        unittest.defaultTestLoader.loadTestsFromModule(__import__(__name__)))
    if not result.wasSuccessful():
        raise SystemExit(1)
    print("vs-osmium driver fixtures ok")
