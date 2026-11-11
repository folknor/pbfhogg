"""Compatibility examples and source-derived ID fixtures for the checker.

The fixture README records provenance and the supported subset. These are not
captured renderer output or proof of full renderer parity.
The doc-links-fixtures script check in brokkr.d/lints.toml runs this file.
"""

import json
import pathlib
import unittest
from unittest.mock import patch

import check_doc_links as checker


class RendererCorpus(unittest.TestCase):
    def test_committed_ids(self):
        corpus = json.loads((pathlib.Path(__file__).parent / 'doc_links_fixtures'
                             / 'anchors.json').read_text(encoding='utf-8'))
        package = json.loads((checker.ROOT / 'package.json').read_text(encoding='utf-8'))
        self.assertEqual(corpus['vitepress_version'], package['devDependencies']['vitepress'],
                         'Re-derive the corpus from the newly pinned VitePress source')
        for renderer in ('github', 'vitepress'):
            for case in corpus[renderer]:
                with self.subTest(renderer=renderer, markdown=case['markdown']), \
                        patch.object(checker.pathlib.Path, 'read_text',
                                     return_value=case['markdown']), \
                        patch.object(checker, 'ANCHOR_CACHE', {}):
                    self.assertEqual(checker.anchors('example.md', renderer),
                                     set(case['ids']))


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
                             {'repeat', 'repeat-1', 'custom-chosen', 'manual'})

    def test_inline_code_is_literal(self):
        self.assertEqual(checker.strip_inline('`<tag>` `*literal*`'),
                         '<tag> *literal*')

    def test_link_consumers_use_collision_ids(self):
        source = '# Repeat\n\n# Repeat-1\n\n# Repeat'
        with patch.object(checker.pathlib.Path, 'read_text', return_value=source), \
                patch.object(checker, 'ANCHOR_CACHE', {}), \
                patch.object(checker, 'FILE_SET', frozenset({'docs/page.md'})):
            self.assertIsNone(checker.site_link_problem('docs/page.md', '', 'repeat-2'))
            self.assertIn('no such anchor', checker.site_link_problem(
                'docs/page.md', '', 'repeat-3'))

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

    def test_literal_sidebar_contract(self):
        sidebars = (
            """{ '/section/': [{ text: 'Label }', items: [
                { 'link': '/one#part' }, /* link: '/missing' */
                { "link": '/two.html?query=yes' }
            ] }] }""",
            """[{ text: 'Group', items: [{ link: '/one' },
                // link: '/missing'
                { link: '/two.html' }] }]""",
        )
        files = ['docs/index.md', 'docs/one.md', 'docs/two.md',
                 'docs/nav.md', 'docs/section/index.md']
        for sidebar in sidebars:
            config = "export default { nav: [{link: '/nav'}], sidebar: " + sidebar + " }"
            with self.subTest(sidebar=sidebar), \
                    patch.object(checker.pathlib.Path, 'read_text', return_value=config), \
                    patch.object(checker, 'FILES', files), \
                    patch.object(checker, 'FILE_SET', frozenset(files)):
                self.assertEqual(checker.sidebar_coverage(), [
                    f'{page}: no sidebar entry names this page in docs/.vitepress/config.ts'
                    for page in ('docs/nav.md', 'docs/section/index.md')])


if __name__ == '__main__':
    result = unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromModule(
        __import__(__name__)))
    if not result.wasSuccessful():
        raise SystemExit(1)
    print("document checker fixtures ok")
