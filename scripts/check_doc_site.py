#!/usr/bin/env python3
"""Check the built documentation site, and the document checker against it.

scripts/check_doc_links.py models the VitePress renderer so the edit loop runs
without Node. This script runs the real renderer: it builds the site with the
VitePress that package.json and pnpm-lock.yaml pin, reads the generated HTML,
and checks four things there.

  1. links: every internal href on every built page (body links, sidebar,
     navigation, pager) names a built file, and its fragment names an id in
     that file. VitePress slugs an authored fragment when it renders a link
     (its link plugin's normalizeHash), so a fragment that names a manual
     `<a id>` anchor can render as a different, dead one; only the built
     href shows that.
  2. sidebar: every site page except the home page renders a sidebar that
     links to the page itself. This is VitePress's own route selection
     (getSidebar picks one sidebar per route), not mere mention somewhere in
     the config.
  3. headings: for every page built from a markdown file, the heading ids
     check_doc_links.py computes for VitePress equal the ids VitePress
     emitted, in order. A divergence means the checker's model is wrong for
     a construct this site uses.
  4. pages: every site markdown page check_doc_links.py treats as a page was
     built, and no other markdown page under docs/ was, apart from
     docs/public/, which VitePress also renders but the checker exempts.

The build goes to docs/.vitepress/dist, which git ignores. Node and the
pinned dependencies are a requirement, not an optional extra: without them
this check fails and names the install command, never skips. It runs in the
complete gate only (brokkr.d/lints.toml), since the edit-loop checker covers
the same rules without a build.

The last line on success is exactly ``document site ok``.
"""

import html.parser
import pathlib
import shutil
import subprocess
import sys
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))
import check_doc_links as checker  # noqa: E402

DIST = ROOT / "docs" / ".vitepress" / "dist"
BASE = "/pbfhogg/"
INSTALL = "pnpm install --frozen-lockfile"


class Page(html.parser.HTMLParser):
    """Ids, content heading ids in order, hrefs, and the sidebar's hrefs."""

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.ids, self.headings, self.hrefs, self.sidebar = set(), [], [], []
        self.aside_depth = 0

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.add(attrs["id"])
        if "name" in attrs and tag == "a":
            self.ids.add(attrs["name"])
        if tag in ("h1", "h2", "h3", "h4", "h5", "h6") and attrs.get("id"):
            self.headings.append(attrs["id"])
        if tag == "aside":
            if self.aside_depth or "VPSidebar" in (attrs.get("class") or "").split():
                self.aside_depth += 1
        if tag == "a" and attrs.get("href") is not None:
            self.hrefs.append(attrs["href"])
            if self.aside_depth:
                self.sidebar.append(attrs["href"])

    def handle_endtag(self, tag):
        if tag == "aside" and self.aside_depth:
            self.aside_depth -= 1


def build():
    """None on success, else the reason the site could not be built."""
    pnpm = shutil.which("pnpm")
    if shutil.which("node") is None or pnpm is None:
        return f"node and pnpm are required; install them, then run {INSTALL}"
    out = subprocess.run(
        [pnpm, "exec", "vitepress", "build", "docs"],
        cwd=ROOT, capture_output=True, text=True, check=False,
    )
    if out.returncode != 0:
        tail = "\n".join((out.stdout + out.stderr).strip().splitlines()[-20:])
        return f"vitepress build failed (dependencies missing? run {INSTALL}):\n{tail}"
    return None


def html_for(rel):
    """The built file for site markdown page `rel` (docs/x/y.md)."""
    return DIST / (rel[len("docs/"):-len(".md")] + ".html")


def target_file(page, href):
    """The built file an internal href names, or None if it is external."""
    url = urllib.parse.urlsplit(href)
    if url.scheme or url.netloc:
        return None
    path = urllib.parse.unquote(url.path)
    if not path:
        return page
    if path.startswith("/"):
        if not path.startswith(BASE):
            return DIST.parent  # outside the base: reported as outside the site
        candidate = DIST / path[len(BASE):]
    else:
        candidate = page.parent / path
    candidate = pathlib.Path(*candidate.parts)
    if path.endswith("/"):
        return candidate / "index.html"
    if candidate.suffix == "" and not candidate.is_file():
        return candidate.with_suffix(".html")
    return candidate


def main():
    problem = build()
    if problem:
        print(problem)
        return 1

    parsed = {}
    for path in sorted(DIST.rglob("*.html")):
        page = Page()
        page.feed(path.read_text(encoding="utf-8"))
        parsed[path.resolve()] = page

    links, sidebar, headings, pages = [], [], [], []
    for path, page in parsed.items():
        shown = path.relative_to(DIST)
        for href in page.hrefs:
            target = target_file(path, href)
            if target is None:
                continue
            target = target.resolve()
            if not target.is_relative_to(DIST.resolve()):
                links.append(f"{shown}: {href}: outside the built site")
                continue
            fragment = urllib.parse.unquote(urllib.parse.urlsplit(href).fragment)
            # A text directive (`#id:~:text=...`) targets the element id
            # before it, or none at all.
            fragment = fragment.split(":~:", 1)[0]
            if target.suffix == ".html":
                if target not in parsed:
                    links.append(f"{shown}: {href}: no such page")
                elif fragment and fragment not in parsed[target].ids:
                    links.append(f"{shown}: {href}: no such anchor")
            elif not target.is_file():
                links.append(f"{shown}: {href}: no such file")

    site_pages = [
        rel for rel in checker.FILES
        if checker.in_site(rel) and rel.endswith(".md")
    ]
    for rel in site_pages:
        built = html_for(rel).resolve()
        if built not in parsed:
            pages.append(f"{rel}: not built")
            continue
        page = parsed[built]
        if rel != "docs/index.md":
            own = {
                target_file(built, href).resolve()
                for href in page.sidebar
                if target_file(built, href) is not None
            }
            if built not in own:
                sidebar.append(f"{rel}: its rendered sidebar does not link to it")
        source = (checker.ROOT / rel).read_text(encoding="utf-8")
        manual = set()
        for line in checker.markdown_lines(source):
            manual.update(checker.HTML_ANCHOR_RE.findall(line))
        computed = checker.heading_ids(rel, "vitepress")
        if computed != page.headings:
            headings.append(
                f"{rel}: checker {computed} != rendered {page.headings}"
            )
        missing = manual - page.ids
        if missing:
            headings.append(f"{rel}: manual anchors not rendered {sorted(missing)}")

    built_md = {
        str(path.relative_to(DIST).with_suffix(".md"))
        for path in parsed
        if path.name != "404.html"
    }
    for name in sorted(built_md):
        rel = f"docs/{name}"
        if rel not in site_pages and not rel.startswith("docs/public/"):
            pages.append(f"{rel}: built but not a page the checker models")

    checks = (
        ("links", links),
        ("sidebar", sidebar),
        ("headings", headings),
        ("pages", pages),
    )
    for label, rows in checks:
        print(f"== {label}: {len(rows)}")
        for row in rows:
            print(f"   {row}")
    if any(rows for _, rows in checks):
        return 1
    print("document site ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
