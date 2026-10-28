#!/usr/bin/env python3
"""Lint the document-folder conventions from AGENTS.md ("Document folders").

Three checks, over the files git tracks or would track (``git ls-files --cached
--others --exclude-standard``), so build output, ``data/``, ``research/`` and
other ignored trees never matter:

  1. durable-cites-notes: nothing durable cites ``notes/``. Durable is
     everything outside ``notes/`` except the root-level files, which AGENTS.md
     exempts (AGENTS.md, CLAUDE.md, README.md, CHANGELOG.md and their kin):
     the folders govern documents we chose where to put, not files whose
     location is dictated. Markdown and Rust are read in full, other text files
     (scripts, TOML, workflows) on their comment lines only. Naming the folder
     itself, a bare `notes/`, describes the layout and is allowed; any path or
     glob under it is a citation.

  2. dangling: a durable file names a repository path that does not exist.
     Three shapes are claimed, each chosen so a false positive needs an
     authored token that looks like a repository path and is not one:

       - a markdown link or reference definition, resolved from the citing
         file's directory. Under ``docs/`` links follow VitePress rules: a
         leading ``/`` is the site root (``docs/``, then ``docs/public/``), and
         an extensionless target may name ``<target>.md`` or
         ``<target>/index.md``.
       - a backticked path with at least one ``/`` whose first component is a
         tracked top-level directory, or the gitignored ``.plans/`` (and the
         retired ``plans/``), which no other checkout can resolve. A trailing
         ``:LINE``, ``::item`` or ``#anchor`` is ignored. A relative spelling
         such as `altw/mod.rs` is not claimed: its base is unstated.
       - a root-level ``UPPERCASE.md`` name, backticked or bare, or a bare
         (unbackticked) path into ``reference/``, ``docs/`` or
         ``decisions/``.

     A path git ignores (build output such as ``docs/.vitepress/dist/``) is
     not claimed, nor is a template placeholder such as ``NNNN-title.md``. In
     code files only comment text is read: a string literal legitimately
     carries a path that need not exist, such as a test's invented argv.

     A link's ``#fragment`` is checked too when the link resolves to a
     markdown file, a same-page ``#fragment`` included: it must name a
     heading (or an explicit ``{#id}`` or ``<a id=...>``/``<a name=...>``
     anchor) of that file, slugged the way the page that receives the reader
     renders it. A link from a site page to a site page lands on VitePress,
     every other link on GitHub, and the two slug rules differ (VitePress
     collapses a run of punctuation into one hyphen, GitHub keeps each
     hyphen). A link to this repository's GitHub URL (``blob/main/PATH`` or
     ``tree/main/PATH``) is resolved against the checkout like a relative
     one: the site cannot link outside ``docs/`` any other way, so those
     links are where it cites the root documents.

     A link from a site page to a site page must also work on GitHub, which
     renders the same files: a relative path naming the target's ``.md``
     file, inside ``docs/``, with a fragment that both slug rules produce.
     A site-root or extensionless page link, one that climbs out of the
     site, or a GitHub URL naming a site page, is reported even when the
     file exists.

  3. source-comment-citations: comments under ``src/`` do not cite a Rust
     source line number. It loses its context as code moves; the comment must
     carry the needed context inline. (The work-item list lives in ``notes/``,
     so check 1 refuses a citation of it.)

Root-level files and ``notes/`` are not scanned at all: the first are exempt,
the second carries no truth guarantee, so a dead link there is its own business.
The exception is ``CORRECTNESS.md`` and ``DEVIATIONS.md``: they sit at the root
but are durable documents rather than tooling-found convention files, and the
site's correctness and deviations pages point at them instead of carrying
copies, so they are held to every check above.

The last line on success is exactly ``document conventions ok``. brokkr's
script_check matches the last line and ignores the exit code, so the sentinel is
printed only when every check is clean; a per-check ``== label: 0`` line can
never be last on a failing run.
"""

import pathlib
import re
import subprocess
import sys
import unicodedata
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Extensions read at all. Everything else (PBF fixtures, fonts, SVG, the
# results database) is data.
MARKDOWN = {".md"}
RUST = {".rs"}
# Other authored text, read on comment lines only.
COMMENTED = {".py", ".sh", ".toml", ".ts", ".yml", ".yaml", ".java", ".css"}
DURABLE_DOC_DIRS = ("reference", "docs")
# Root-level documents that are durable, not convention files: the single
# source the site's pages link to (see the module docstring).
DURABLE_ROOT_DOCS = frozenset({"CORRECTNESS.md", "DEVIATIONS.md"})
# This repository on GitHub. A link of this shape names a checkout path.
GITHUB_DOC_RE = re.compile(
    r"^https://github\.com/folknor/pbfhogg/(?:blob|tree)/main/([^?#]*)(?:\?[^#]*)?(?:#(.*))?$"
)
ATX_HEADING_RE = re.compile(r"^ {0,3}#{1,6}[ \t]+(.*?)(?:[ \t]+#+)?[ \t]*$")
EXPLICIT_ID_RE = re.compile(r"\s*\{#([^}\s]+)\}\s*$")
HTML_ANCHOR_RE = re.compile(r"<a\s[^>]*\b(?:id|name)\s*=\s*[\"']([^\"']+)[\"']", re.I)

# Top-level names claimed by the backticked-path leg even though git does not
# track them: `.plans/` is gitignored local scratch and `plans/` the retired
# folder, so a citation of either dangles on every other checkout.
UNTRACKED_CLAIMED_ROOTS = {".plans", "plans"}

# The notes folder and everything under it. The lookbehind refuses a match
# that is the tail of a longer path (another directory's notes, a URL), but a
# leading run of `./` or `../` is captured so a relative markdown link that
# climbs into the folder is still seen.
NOTES_RE = re.compile(r"(?<![\w./-])((?:\.\.?/)*)notes/([\w.*/-]*)")
MD_LINK_RE = re.compile(
    r"!?\[((?:[^\[\]]|\[[^\]]*\])*)\]\(\s*<?([^)\s>]+)>?(?:\s+(?:\"[^\"]*\"|'[^']*'))?\s*\)"
)
MD_REFDEF_RE = re.compile(r"^ {0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s|$)", re.M)
CODE_SPAN_RE = re.compile(r"(`+)(.+?)(?<!`)\1(?!`)", re.S)
SPAN_PATH_RE = re.compile(
    r"^(?:\./)?([A-Za-z0-9_.][A-Za-z0-9_.-]*(?:/[A-Za-z0-9_.-]+)+/?)"
    r"(?::\d+(?:-\d+)?|::[\w:]+)?(?:#[\w.-]+)?$"
)
BARE_PATH_RE = re.compile(
    r"(?<![\w./-])((?:reference|docs|decisions)/[\w./-]*[\w/])"
    r"|(?<![\w./-])([A-Z][A-Z0-9_-]*\.md)\b"
)
ROOT_DOC_RE = re.compile(r"^[A-Z][A-Z0-9_-]*\.md$")
PLACEHOLDER_RE =re.compile(r"N{3,}|X{3,}")
SOURCE_COMMENT_CITATION_RE = re.compile(r"\.rs:[0-9]+")
SCHEME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
FENCE_RE = re.compile(r"^ {0,3}(```|~~~)")


def git(*args, stdin=None):
    return subprocess.run(
        ["git", *args],
        cwd=ROOT,
        input=stdin,
        capture_output=True,
        text=True,
        check=False,
    )


def repository_files():
    out = git("ls-files", "-z", "--cached", "--others", "--exclude-standard")
    if out.returncode != 0:
        raise SystemExit(f"git ls-files failed: {out.stderr.strip()}")
    names = sorted(set(out.stdout.split("\0")) - {""})
    # A deletion not yet staged is still in the index; it does not exist.
    return [n for n in names if (ROOT / n).is_file()]


FILES = repository_files()
FILE_SET = frozenset(FILES)
DIR_SET = frozenset(
    str(parent)
    for name in FILES
    for parent in pathlib.PurePosixPath(name).parents
    if str(parent) != "."
)
TOP_DIRS = frozenset(name.split("/", 1)[0] for name in FILES if "/" in name)


def exists(target):
    target = target.rstrip("/")
    return target in FILE_SET or target in DIR_SET


def ignored(paths):
    """The subset of `paths` git ignores (build output, local data)."""
    if not paths:
        return set()
    out = git("check-ignore", "--no-index", "--stdin", stdin="\n".join(paths) + "\n")
    return set(out.stdout.splitlines())


def normalize(path):
    """Collapse `.` and `..` in a repository-relative path; None if it leaves
    the repository."""
    parts = []
    for part in path.split("/"):
        if part in ("", "."):
            continue
        if part == "..":
            if not parts:
                return None
            parts.pop()
        else:
            parts.append(part)
    return "/".join(parts)


def parent_of(rel):
    return str(pathlib.PurePosixPath(rel).parent)


def is_durable(rel):
    if rel in DURABLE_ROOT_DOCS:
        return True
    return "/" in rel and not rel.startswith("notes/")


def comment_text(line, suffix):
    """The comment portion of a code line, or None if it has none."""
    stripped = line.lstrip()
    if suffix in (".rs", ".ts", ".java", ".css"):
        if stripped.startswith(("/*", "*")):
            return stripped
        index = line.find("//")
        # `https://` inside a string is not a comment opener.
        while index > 0 and line[index - 1] == ":":
            index = line.find("//", index + 2)
        return None if index < 0 else line[index:]
    if stripped.startswith("#"):
        return stripped
    index = line.find(" #")
    return None if index < 0 else line[index:]


# ---------------------------------------------------------------- check 1


def notes_citations(rel, line):
    found = []
    for match in NOTES_RE.finditer(line):
        lead, rest = match.group(1), match.group(2).rstrip(".,;:")
        if not rest:
            continue  # the folder named as a folder
        if ".." in lead:
            resolved = normalize(f"{parent_of(rel)}/{lead}notes/{rest}")
            if resolved is None or not resolved.startswith("notes/"):
                continue
        found.append(f"notes/{rest}")
    return found


# ---------------------------------------------------------------- check 2


def in_site(rel):
    return rel.startswith("docs/") and not rel.startswith("docs/public/")


def resolve_link(rel, target):
    """The repository path a markdown link from `rel` names, and whether it
    exists. For a site link to a page the path is the markdown file VitePress
    serves for it."""
    site = in_site(rel)
    if target.startswith("/"):
        if not site:
            return target, False
        candidates = [normalize("docs" + target), normalize("docs/public" + target)]
    else:
        candidates = [normalize(f"{parent_of(rel)}/{target}")]
    for candidate in candidates:
        if candidate is None:
            continue
        if site:
            stem = candidate[:-5] if candidate.endswith(".html") else candidate
            for page in (f"{stem}.md", f"{stem}/index.md"):
                if page in FILE_SET:
                    return page, True
        if exists(candidate):
            return candidate, True
    return candidates[0], False


def blank(text, match, group=0):
    start, end = match.span(group)
    return text[:start] + "".join("\n" if c == "\n" else " " for c in text[start:end]) + text[end:]


def link_targets(text):
    """(offset, target, fragment) for every local markdown link, and `text`
    with the links blanked so their text is not read again as a path citation
    (the link target is the citation; a backticked copy of it in the link text
    is not a second one). A same-page link has an empty target; a link to this
    repository on GitHub has a target of the form `github:PATH`."""
    found = []
    for match in MD_LINK_RE.finditer(text):
        found.append((match.start(), match.group(2)))
    for match in MD_REFDEF_RE.finditer(text):
        found.append((match.start(), match.group(1)))
    for match in reversed(list(MD_LINK_RE.finditer(text))):
        text = blank(text, match)
    out = []
    for offset, target in found:
        github = GITHUB_DOC_RE.match(target)
        if github:
            path = urllib.parse.unquote(github.group(1)).rstrip("/")
            fragment = urllib.parse.unquote(github.group(2) or "")
            out.append((offset, f"github:{path}", fragment))
            continue
        if SCHEME_RE.match(target) or target.startswith("//"):
            continue
        path, _, fragment = target.partition("#")
        path = urllib.parse.unquote(path.split("?", 1)[0])
        fragment = urllib.parse.unquote(fragment)
        if path or fragment:
            out.append((offset, path, fragment))
    return out, text


# ---------------------------------------------------------------- anchors


def strip_inline(text):
    """Heading text as rendered: links reduced to their text, images
    dropped, HTML tags and emphasis markers removed, code spans kept as
    their literal content."""
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", "", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"<[^>]+>", "", text)
    spans = []

    def keep(match):
        spans.append(match.group(2).strip())
        return f"\0{len(spans) - 1}\0"

    text = CODE_SPAN_RE.sub(keep, text)
    text = re.sub(r"\*+|(?<!\w)_+|_+(?!\w)", "", text)
    return re.sub(r"\0(\d+)\0", lambda m: spans[int(m.group(1))], text)


def github_slug(text):
    """GitHub's heading id: lowercase, every character that is not a letter,
    digit, space, hyphen or underscore dropped, each space a hyphen."""
    text = text.strip().lower()
    text = "".join(c for c in text if c.isalnum() or c in " -_")
    return text.replace(" ", "-")


# Spelled as code points so this file carries no typographic characters.
CURLY_QUOTES = "".join(map(chr, (0x201C, 0x201D, 0x2018, 0x2019)))
COMBINING_RE = re.compile("[" + chr(0x300) + "-" + chr(0x36F) + "]")
VITEPRESS_SPECIAL_RE = re.compile(
    "[\\s~`!@#$%^&*()\\-_+=\\[\\]{}|\\\\;:\"'" + CURLY_QUOTES + "<>,.?/]+"
)


def vitepress_slug(text):
    """VitePress's heading id (the @mdit-vue slugify): accents decomposed and
    dropped, each run of whitespace and ASCII punctuation one hyphen, no
    hyphen at either end, a leading digit prefixed with an underscore."""
    text = unicodedata.normalize("NFKD", text)
    text = COMBINING_RE.sub("", text)
    text = re.sub("[\u0000-\u001f]", "", text)
    text = VITEPRESS_SPECIAL_RE.sub("-", text)
    text = re.sub(r"-{2,}", "-", text).strip("-")
    text = re.sub(r"^(\d)", r"_\1", text)
    return text.lower()


ANCHOR_CACHE = {}


def anchors(rel, renderer):
    """Every fragment a reader can land on in markdown file `rel` under the
    given renderer (`github` or `vitepress`). Headings inside fenced code are
    not headings; a repeated heading gets `-1`, `-2`, ... as both renderers
    number it."""
    key = (rel, renderer)
    if key in ANCHOR_CACHE:
        return ANCHOR_CACHE[key]
    slug = github_slug if renderer == "github" else vitepress_slug
    found, seen, fenced = set(), {}, False
    for line in (ROOT / rel).read_text(encoding="utf-8").splitlines():
        if FENCE_RE.match(line):
            fenced = not fenced
            continue
        if fenced:
            continue
        found.update(HTML_ANCHOR_RE.findall(line))
        match = ATX_HEADING_RE.match(line)
        if not match:
            continue
        text = match.group(1)
        explicit = EXPLICIT_ID_RE.search(text)
        if explicit and renderer == "vitepress":
            found.add(explicit.group(1))
            continue
        if explicit:
            text = text[: explicit.start()]
        base = slug(strip_inline(text))
        count = seen.get(base, 0)
        seen[base] = count + 1
        found.add(base if count == 0 else f"{base}-{count}")
    ANCHOR_CACHE[key] = found
    return found


def span_paths(text):
    for span in CODE_SPAN_RE.finditer(text):
        content = span.group(2).strip()
        if ROOT_DOC_RE.match(content):
            yield span.start(), content
            continue
        match = SPAN_PATH_RE.match(content)
        if not match:
            continue
        path = match.group(1)
        top = path.split("/", 1)[0]
        if top in TOP_DIRS or top in UNTRACKED_CLAIMED_ROOTS:
            yield span.start(), path


def bare_paths(text):
    # Backticked spans are the other leg's; blank them so a path is not
    # reported twice.
    text = CODE_SPAN_RE.sub(lambda m: "".join("\n" if c == "\n" else " " for c in m.group(0)), text)
    for match in BARE_PATH_RE.finditer(text):
        before = text[: match.start()].rsplit(None, 1)
        if before and "://" in before[-1]:
            continue
        path = (match.group(1) or match.group(2)).rstrip(".")
        if "*" in path:
            continue
        yield match.start(), path


PAGE_SUFFIXES = ("", ".md", ".html")


def site_link_problem(rel, target, fragment):
    """Why a link on a site page cannot be followed on both of the site's
    reading surfaces, or None. A `docs/` page is rendered by VitePress and,
    being a repository file, by GitHub too, so a link from one site page to
    another must work in both: a relative path naming the target's `.md`
    file (`index.md` for a directory), never a site-root `/path` or an
    extensionless one, which only VitePress resolves; a path that stays
    inside the site, since VitePress cannot serve what lies outside `docs/`
    (the rest of the repository is linked by its GitHub URL); and a
    fragment that names the same heading under both slug rules. Assets
    (images, downloads) are not pages and are left to the dangling check."""
    path = target
    if path.startswith("github:"):
        named = normalize(path[len("github:") :]) or ""
        if named.endswith(".md") and in_site(named):
            return "a site page on GitHub; link it by its relative .md path"
        return None
    if path and pathlib.PurePosixPath(path).suffix not in PAGE_SUFFIXES:
        return None
    if path and not path.endswith(".md"):
        # An extensionless target that names an existing non-page file (a
        # download under docs/public/) is an asset, not a page.
        resolved, ok = resolve_link(rel, path)
        if ok and not resolved.endswith(".md"):
            return None
    if path:
        if path.startswith("/"):
            return "site-root link; name the page's .md file by a relative path"
        if not path.endswith(".md"):
            return "name the page's .md file (index.md for a directory)"
        resolved = normalize(f"{parent_of(rel)}/{path}")
        if resolved is None or not in_site(resolved):
            return "leaves the site; link the repository file by its GitHub URL"
        if resolved not in FILE_SET:
            return None  # the dangling check reports it
    else:
        resolved = rel
    if fragment:
        missing = [
            renderer
            for renderer in ("vitepress", "github")
            if fragment not in anchors(resolved, renderer)
        ]
        if missing:
            return f"no such anchor under the {' and '.join(missing)} slug rule"
    return None


def dangling_paths(rel, text, markdown):
    """(offset, description) for each path named in `text` that does not
    exist. Paths into the notes folder are check 1's, which already refuses
    them in a durable file."""
    out = []
    if markdown:
        links, text = link_targets(text)
        for offset, target, fragment in links:
            shown = target + (f"#{fragment}" if fragment else "")
            if in_site(rel):
                problem = site_link_problem(rel, target, fragment)
                if problem:
                    out.append((offset, f"link {shown}: {problem}", None))
                    continue
            if target.startswith("github:"):
                resolved = normalize(target[len("github:") :]) or ""
                ok = exists(resolved)
                renderer = "github"
            elif not target:
                resolved, ok = rel, True
                renderer = "vitepress" if in_site(rel) else "github"
            else:
                resolved, ok = resolve_link(rel, target)
                renderer = "vitepress" if in_site(rel) and in_site(resolved) else "github"
            if not ok:
                if not resolved.startswith("notes/"):
                    out.append((offset, f"link {shown}", resolved))
                continue
            if fragment and resolved.endswith(".md") and resolved in FILE_SET:
                if fragment not in anchors(resolved, renderer):
                    # No path: an ignore rule never excuses a missing anchor.
                    out.append((offset, f"link {shown}: no such anchor", None))
    for offset, path in [*span_paths(text), *bare_paths(text)]:
        if path.startswith("notes/") or PLACEHOLDER_RE.search(path):
            continue
        if not exists(path):
            out.append((offset, path, path))
    return out


def markdown_paragraphs(text):
    """(first line number, paragraph) for each run of non-blank lines outside
    fenced code. Paragraphs, not lines, because a code span or a link may wrap
    across lines, and reading line by line pairs the wrong backticks."""
    block, start, fenced = [], 0, False
    for num, line in enumerate(text.splitlines(), 1):
        if FENCE_RE.match(line):
            fenced = not fenced
            line = ""
        elif fenced:
            line = ""
        if line.strip():
            if not block:
                start = num
            block.append(line)
        elif block:
            yield start, "\n".join(block)
            block = []
    if block:
        yield start, "\n".join(block)


# ------------------------------------------------------------------- main


def main():
    transient_cites = []
    source_comment_cites = []
    candidates = []  # (row, repository path used for the ignore test)
    durable_docs = 0
    for rel in FILES:
        if not is_durable(rel):
            continue
        suffix = pathlib.PurePosixPath(rel).suffix
        if suffix not in MARKDOWN | RUST | COMMENTED:
            continue
        try:
            text = (ROOT / rel).read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if suffix in MARKDOWN:
            if rel.startswith(DURABLE_DOC_DIRS):
                durable_docs += 1
            for num, line in enumerate(text.splitlines(), 1):
                for cite in notes_citations(rel, line):
                    transient_cites.append(f"{rel}:{num}: {cite}")
            for start, para in markdown_paragraphs(text):
                for offset, desc, path in dangling_paths(rel, para, markdown=True):
                    num = start + para.count("\n", 0, offset)
                    candidates.append((f"{rel}:{num}: {desc}", path))
            continue
        for num, line in enumerate(text.splitlines(), 1):
            comment = comment_text(line, suffix)
            scope = line if suffix in RUST else comment
            if scope is not None:
                for cite in notes_citations(rel, scope):
                    transient_cites.append(f"{rel}:{num}: {cite}")
            if comment is not None:
                if rel.startswith("src/") and suffix == ".rs":
                    for match in SOURCE_COMMENT_CITATION_RE.finditer(comment):
                        source_comment_cites.append(
                            f"{rel}:{num}: {match.group(0)}"
                        )
                for _, desc, path in dangling_paths(rel, comment, markdown=False):
                    candidates.append((f"{rel}:{num}: {desc}", path))

    # Fail closed: a renamed folder or a broken file listing would otherwise
    # print a clean pass over nothing.
    if durable_docs == 0:
        print(
            f"no durable documents found under {', '.join(DURABLE_DOC_DIRS)}"
            " - a pass over zero documents is not a pass"
        )
        return 1

    skip = ignored(
        [
            p
            for _, p in candidates
            if p is not None and p.split("/", 1)[0] not in UNTRACKED_CLAIMED_ROOTS
        ]
    )
    dangling = [row for row, path in candidates if path is None or path not in skip]

    checks = (
        ("durable-cites-notes", transient_cites),
        ("dangling", dangling),
        ("source-comment-citations", source_comment_cites),
    )
    for label, rows in checks:
        print(f"== {label}: {len(rows)}")
        for row in rows:
            print(f"   {row}")
    if any(rows for _, rows in checks):
        return 1
    print("document conventions ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
