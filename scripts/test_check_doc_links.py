"""In-memory compatibility examples for the document checker.

These authored expectations cover the supported subset. They are not captured
renderer output and must not be presented as proof of full renderer parity.
The doc-links-fixtures script check in brokkr.d/lints.toml runs this file.
"""

import unittest
from unittest.mock import patch

import check_doc_links as checker


class Fences(unittest.TestCase):
    def test_delimiters(self):
        cases = (
            ("````\n```\n[hidden](missing.md)\n````\n[real](real.md)", 5),
            ("~~~\n```\n[hidden](missing.md)\n~~~\n[real](real.md)", 5),
            ("```\n~~~\n[hidden](missing.md)\n```\n[real](real.md)", 5),
            ("```\n``` suffix\n[hidden](missing.md)\n````\n[real](real.md)", 5),
            ("   ``` python\n[hidden](missing.md)\n   ```\n[real](real.md)", 4),
            ("~~~~\n~~~\n[hidden](missing.md)\n~~~~~\n[real](real.md)", 5),
        )
        for source, line in cases:
            with self.subTest(source=source):
                self.assertEqual(list(checker.markdown_paragraphs(source)),
                                 [(line, "[real](real.md)")])
                self.assertEqual(list(checker.markdown_headings(
                    source.replace("[hidden](missing.md)", "# Hidden")
                    .replace("[real](real.md)", "# Real"))), ["Real"])

    def test_unclosed_fence_hides_rest(self):
        self.assertEqual(list(checker.markdown_paragraphs("````\n```\n# Hidden")), [])

    def test_invalid_backtick_info_is_text(self):
        source = "``` bad`info\n# Visible"
        self.assertEqual(list(checker.markdown_headings(source)), ["Visible"])

    def test_tilde_info_may_contain_backticks(self):
        self.assertEqual(list(checker.markdown_headings("~~~ `info`\n# Hidden\n~~~")), [])


class Compatibility(unittest.TestCase):
    def test_heading_styles(self):
        source = "# ATX ##\n\nSetext\n======\n\nTwo\nlines\n-----\n\n---\n"
        self.assertEqual(list(checker.markdown_headings(source)),
                         ["ATX", "Setext", "Two\nlines"])

    def test_slug_subset(self):
        cases = (
            ("Hello World", "hello-world", "hello-world"),
            ("A / B", "a--b", "a-b"),
            ("123 name", "123-name", "_123-name"),
            ("caf\u00e9", "caf\u00e9", "cafe"),
            ("under_score", "under_score", "under-score"),
        )
        for text, github, vitepress in cases:
            with self.subTest(text=text):
                self.assertEqual(checker.github_slug(text), github)
                self.assertEqual(checker.vitepress_slug(text), vitepress)

    def test_inline_subset(self):
        self.assertEqual(checker.strip_inline(
            '**Bold** [link](page.md) `x_y` <b>HTML</b>\nnext'),
            'Bold link x_y HTML next')

    def test_anchor_consumers(self):
        source = "# Repeat\n\nRepeat\n======\n\n# Custom {#chosen}\n<a id='manual'>\n"
        with patch.object(checker.pathlib.Path, 'read_text', return_value=source), \
                patch.object(checker, 'ANCHOR_CACHE', {}):
            self.assertEqual(checker.anchors('example.md', 'vitepress'),
                             {'repeat', 'repeat-1', 'chosen', 'manual'})
            self.assertEqual(checker.anchors('example.md', 'github'),
                             {'repeat', 'repeat-1', 'custom', 'manual'})

    def test_link_subset(self):
        links, blanked = checker.link_targets(
            '[page](a.md#part)\n[ref]: b.md#section\n[web](https://example.org)')
        self.assertEqual(links, [(0, 'a.md', 'part'), (18, 'b.md', 'section')])
        self.assertEqual(blanked.count('\n'), 2)

    def test_sidebar_rejects_computed_value(self):
        with patch.object(checker.pathlib.Path, 'read_text',
                          return_value='export default { sidebar: makeSidebar() }'):
            self.assertEqual(checker.sidebar_coverage(), [
                'docs/.vitepress/config.ts: expected one literal sidebar object or array'])


if __name__ == '__main__':
    result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromModule(
        __import__(__name__)))
    if not result.wasSuccessful():
        raise SystemExit(1)
    print("document checker fixtures ok")
