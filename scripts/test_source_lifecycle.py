"""In-memory negative fixtures for the closed lifecycle source shapes."""
import unittest

from rust_source_shapes import FINALIZE, PROBE_OPEN, lifecycle_errors


CLI = """
fn finish_invocation<T>(invocation: &Invocation, work: impl FnOnce() -> T) -> T {
%s
}
fn run() {
    let result = finish_invocation(&host.invocation, || -> Result<(), Box<dyn std::error::Error>> {
        work()
    });
    report(result);
}
""" % FINALIZE
SCRATCH = """
fn open_probe_lock(path: &Path) -> io::Result<File> {
%s
}
fn probe_lock(dir: &Path) -> Probe {
    let lock = dir.join(LOCK_NAME);
    let file = match open_probe_lock(&lock) { Ok(file) => file, Err(e) => return unknown(e) };
    probe(file)
}
""" % PROBE_OPEN


class LifecycleShapes(unittest.TestCase):
    def test_audited_shapes(self):
        self.assertEqual(lifecycle_errors(CLI, SCRATCH), [])

    def test_each_forbidden_probe_option(self):
        for option in ("create(true)", "create(false)", "create_new(true)",
                       "truncate(true)", "write(false)", "read(false)",
                       "custom_flags(libc::O_CREAT)"):
            with self.subTest(option=option):
                changed = SCRATCH.replace(".open(path)", "." + option + ".open(path)")
                self.assertTrue(lifecycle_errors(CLI, changed))

    def test_probe_delegation_and_unrelated_builder(self):
        for body in ("helper(path)", 'let s = ".write(true)"; helper(path)',
                     PROBE_OPEN + "; other(path)"):
            with self.subTest(body=body):
                self.assertTrue(lifecycle_errors(CLI, SCRATCH.replace(PROBE_OPEN, body)))
        self.assertTrue(lifecycle_errors(CLI, SCRATCH.replace(
            "match open_probe_lock(&lock)", "match other(&lock)")))

    def test_fake_counter_calls(self):
        for body in ('let s = r#"' + FINALIZE + '"#; work()',
                     "/* " + FINALIZE + " */ work()",
                     "fn unused() { " + FINALIZE + " } work()",
                     "if false { " + FINALIZE + " } work()",
                     "work()"):
            with self.subTest(body=body):
                self.assertTrue(lifecycle_errors(CLI.replace(FINALIZE, body), SCRATCH))

    def test_dispatch_cannot_hide_in_a_branch_or_closure(self):
        start = "let result = finish_invocation"
        for prefix in ("if false { ", "let unused = || { ", "fn unused() { "):
            with self.subTest(prefix=prefix):
                changed = CLI.replace(start, prefix + start).replace(
                    "report(result);", "} report(result);")
                self.assertTrue(lifecycle_errors(changed, SCRATCH))

    def test_missing_and_duplicate_helpers(self):
        self.assertTrue(lifecycle_errors(CLI + CLI, SCRATCH))
        self.assertTrue(lifecycle_errors(CLI, SCRATCH + SCRATCH))
        self.assertTrue(lifecycle_errors("", ""))

    def test_comments_and_layout_do_not_change_valid_shape(self):
        self.assertEqual(lifecycle_errors(
            CLI.replace("let result", "/* nested /* comment */ */ let\nresult"),
            SCRATCH.replace(".write(true)", ".write( /* writing */ true )")), [])


if __name__ == "__main__":
    result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromModule(
        __import__(__name__)))
    if not result.wasSuccessful():
        raise SystemExit(1)
    print("source lifecycle fixtures ok")
