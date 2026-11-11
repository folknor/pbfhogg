"""In-memory fixtures for the library's counted static inventory."""
import unittest

from rust_source_shapes import static_inventory_errors, static_items


class StaticInventory(unittest.TestCase):
    def test_nested_modules_and_macro_bodies(self):
        for source in (
            '#[cfg(feature = "test-hooks")] mod renamed { static X: u8 = 0; }',
            '#[cfg(all(test, feature = "test-hooks"))] mod a { mod b { static X: u8 = 0; }}',
            'macro_rules! state { () => { static X: AtomicBool = AtomicBool::new(false); }}',
            'macro_rules! state { ($name:ident) => { static $name: u8 = 0; }}',
            'thread_local! { static X: Cell<u8> = Cell::new(0); }',
            'fn f() { static mut X: u8 = 0; }',
            'pub(crate) static\n X: u8 = 0;',
        ):
            with self.subTest(source=source):
                self.assertEqual(len(static_inventory_errors({"owner": source}, {})), 1)

    def test_comments_literals_lifetimes_and_raw_identifiers(self):
        source = '''// static X: u8 = 0;
        /* static X: u8 = 0; /* nested */ */
        fn f<'static_fake>(x: &'static str) { let r#static = 1; }
        let s = r###"static X: u8 = 0;"###;
        let s = "static X: u8 = 0;";
        '''
        self.assertEqual(list(static_items(source)), [])

    def test_counted_headers_and_presence(self):
        source = 'static X: AtomicBool = AtomicBool::new(false);'
        inventory = {"owner": {"static X: AtomicBool =": 1}}
        self.assertEqual(static_inventory_errors({"owner": source}, inventory), [])
        for sources in ({}, {"owner": ""}, {"owner": source * 2},
                        {"owner": source.replace("AtomicBool", "Other")},
                        {"owner": source.replace("static X", "static mut X")},
                        {"owner": source, "renamed": source}):
            with self.subTest(sources=sources):
                self.assertTrue(static_inventory_errors(sources, inventory))

    def test_offsets_survive_multiline_comments(self):
        self.assertEqual(list(static_items('/* a\n b */\nstatic X: u8 = 0;')),
                         [(3, 'staticX:u8=')])


if __name__ == "__main__":
    result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromModule(
        __import__(__name__)))
    if not result.wasSuccessful():
        raise SystemExit(1)
    print("source static fixtures ok")
