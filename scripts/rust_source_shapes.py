"""Closed Rust source shapes for small audited lifecycle operations.

This is lexical enforcement of complete bodies, not an AST or a claim about
arbitrary Rust control flow. A changed shape requires an explicit audit.
"""
import re


def blank_comments_and_literals(src):
    """Rust source with comments turned to spaces and the contents of string
    and char literals turned to `x`, offsets and newlines preserved.

    Comments vanish (whitespace); literals stay opaque tokens with their
    delimiters, so neither can supply or hide code, and blanking a literal
    cannot join the tokens around it into something that looks like code.
    Handles nested block comments, byte and C strings, raw strings with any
    number of hashes, char and byte literals, and tells lifetimes from chars.
    """
    out = list(src)
    n = len(src)
    i = 0

    def fill(start, end, ch):
        for k in range(start, end):
            if out[k] != "\n":
                out[k] = ch

    while i < n:
        c = src[i]
        if src.startswith("//", i):
            end = src.find("\n", i)
            end = n if end == -1 else end
            fill(i, end, " ")
            i = end
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            fill(i, j, " ")
            i = j
        elif (m := re.compile(r'(?:b|c)?r(#*)"').match(src, i)) and (
            i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")
        ):
            close = '"' + m.group(1)
            end = src.find(close, m.end())
            end = n if end == -1 else end + len(close)
            fill(m.end(), end - len(close), "x")
            i = end
        elif c == '"' or (
            c in "bc"
            and src.startswith('"', i + 1)
            and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))
        ):
            start = i + 1 if c == '"' else i + 2
            j = start
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            fill(start, j, "x")
            i = j + 1
        elif c == "'" or (
            c == "b"
            and src.startswith("'", i + 1)
            and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))
        ):
            q = i if c == "'" else i + 1
            m = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'\n])'").match(src, q)
            if m:
                fill(q + 1, m.end() - 1, "x")
                i = m.end()
            else:
                i = q + 1  # a lifetime or label
        else:
            i += 1
    return "".join(out)


def compact(source):
    return re.sub(r"\s+", "", blank_comments_and_literals(source))


def function_body(source, name):
    """One brace-balanced function body, with comments and literals masked.

    This is a deliberately closed source shape, not a Rust parser. Requiring
    the complete tiny body rejects delegation, extra flags and dead branches
    rather than trying to infer their effects from token presence.
    """
    code = blank_comments_and_literals(source)
    matches = list(re.finditer(r"\bfn\s+" + re.escape(name) + r"\s*(?:<[^{}]*>)?\s*\(", code))
    if len(matches) != 1:
        return None
    start = code.find("{", matches[0].end())
    if start == -1:
        return None
    depth = 1
    for end in range(start + 1, len(code)):
        depth += (code[end] == "{") - (code[end] == "}")
        if depth == 0:
            return code[start + 1:end]
    return None


PROBE_OPEN = """
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
"""
FINALIZE = """
    let result = work();
    pbfhogg::write::emit_writer_counters(invocation);
    pbfhogg::read::emit_read_counters(invocation);
    result
"""


def sealed_body(source, name, expected):
    body = function_body(source, name)
    return body is not None and compact(body) == compact(expected)


def lifecycle_errors(cli, scratch):
    found = []
    if not sealed_body(scratch, "open_probe_lock", PROBE_OPEN):
        found.append("scratch probe constructor must use exactly the audited noncreating option chain")
    probe = function_body(scratch, "probe_lock")
    # Seal the open expression at the match, not an unrelated builder or a
    # string elsewhere in the function. No second opener is admitted.
    if probe is None or not re.search(
        r"letfile=matchopen_probe_lock\(&lock\)\{", compact(probe)
    ) or re.search(r"OpenOptions|File::open|\.open\(", compact(probe)):
        found.append("probe_lock must open its descriptor through open_probe_lock only")
    if not sealed_body(cli, "finish_invocation", FINALIZE):
        found.append("CLI finalization must run work and then both counter hooks unconditionally")
    run = function_body(cli, "run")
    if run is None:
        found.append("CLI run function missing")
    else:
        code = compact(run)
        call = "letresult=finish_invocation(&host.invocation,||->Result<(),Box<dynstd::error::Error>>{"
        start = code.find(call)
        # The dispatch owner must be a direct statement in run, not inside
        # an unused closure, nested function or unreachable branch.
        if start == -1 or code[:start].count("{") != code[:start].count("}"):
            found.append("CLI run must dispatch directly through finish_invocation")
        if code.count("finish_invocation(") != 1:
            found.append("CLI run must finalize exactly one dispatch")
        if "emit_writer_counters(" in code or "emit_read_counters(" in code:
            found.append("CLI run counter hooks belong in finish_invocation")
    return found

