"""Audit the closed integration-test module shape, without building Rust.

External modules must be at file scope. Inline modules may contain tests, but
not external module declarations. Unknown cfg forms fail closed; extend the
small evaluator deliberately when the test graph needs a new form. This checks
source reachability and module presence in at least one executable sweep,
not macro expansion or whether a test executes.
"""
from pathlib import Path
import re
import sys
import tomllib

from rust_source_shapes import blank_comments_and_literals


def cfg_value(expression, features):
    tokens = re.findall(r'"[^"]*"|[A-Za-z_][\w-]*|[(),=]', expression)
    position = 0

    def parse():
        nonlocal position
        name = tokens[position]
        position += 1
        if name in ('all', 'any', 'not'):
            assert tokens[position] == '('
            position += 1
            values = []
            while tokens[position] != ')':
                values.append(parse())
                if tokens[position] == ',':
                    position += 1
                else:
                    break
            assert tokens[position] == ')'
            position += 1
            if name == 'not':
                assert len(values) == 1
                return not values[0]
            return all(values) if name == 'all' else any(values)
        if name in ('test', 'unix'):
            return True
        assert name in ('feature', 'target_os', 'target_env', 'target_endian', 'target_arch'), \
            f'unsupported cfg: {name}'
        assert tokens[position] == '='
        value = tokens[position + 1].strip('"')
        position += 2
        if name == 'feature':
            return value in features
        if name == 'target_endian':
            return value == 'little'
        if name == 'target_arch':
            return value == 'x86_64'
        # These are the executable Linux sweeps configured by this project.
        return value in features if name == 'target_env' else value == 'linux'

    result = parse()
    assert position == len(tokens), f'unsupported cfg expression: {expression}'
    return result


def module_edges(source, base, features, directory=None):
    """Module files declared at file scope. `base` resolves default module
    names; a #[path] attribute outside inline modules resolves against the
    declaring file's own `directory` (Rust reference, path attribute), which
    differs from `base` for a non-mod.rs file."""
    if directory is None:
        directory = base
    code = blank_comments_and_literals(source)
    pattern = r'((?:#\s*\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;'
    for match in re.finditer(pattern, code):
        prefix = code[:match.start()]
        assert prefix.count('{') == prefix.count('}'), 'external module inside inline body needs explicit audit support'
        attrs = source[match.start(1):match.end(1)]
        assert 'cfg_attr' not in attrs, 'cfg_attr modules need explicit audit support'
        gates = re.findall(r'#\s*\[cfg\((.*?)\)\]', attrs, re.S)
        # Evaluate all gates, even after a false gate, to reject unknown syntax.
        enabled = [cfg_value(gate, features) for gate in gates]
        if not all(enabled):
            continue
        paths = re.findall(r'#\s*\[path\s*=\s*"([^"]+)"\s*\]', attrs)
        assert len(paths) <= 1
        if paths:
            yield (directory / paths[0]).resolve()
        else:
            candidates = [base / (match[2] + '.rs'), base / match[2] / 'mod.rs']
            existing = [path.resolve() for path in candidates if path.is_file()]
            assert len(existing) == 1, f'module {match[2]}: expected exactly one of {candidates}'
            yield existing[0]


def reachable(roots, features):
    visited = set()
    pending = list(roots)
    while pending:
        path = pending.pop()
        if path in visited:
            continue
        assert path.is_file(), f'declared module missing: {path}'
        source = path.read_text()
        code = blank_comments_and_literals(source)
        inner = [source[match.start():match.end()] for match in
                 re.finditer(r'#!\s*\[[^\]]*\]', code)]
        assert not any('cfg_attr' in attr for attr in inner), 'inner cfg_attr needs explicit audit support'
        gates = [cfg_value(gate, features) for attr in inner
                 for gate in re.findall(r'#!\s*\[cfg\((.*?)\)\]', attr, re.S)]
        if not all(gates):
            continue
        visited.add(path)
        base = path.parent if path.name == 'mod.rs' or path in roots else path.with_suffix('')
        pending.extend(module_edges(source, base, features, path.parent))
    return visited


def module_presence(source, features):
    """Declaration identities, including inline modules and enclosing cfg gates.

    File-level gates are handled by reachable(). This lexical check shares its
    closed cfg evaluator and intentionally makes no claim about macro expansion.
    """
    code = blank_comments_and_literals(source)
    pattern = r'((?:#\s*\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*([;{])'
    scopes = []
    declared, enabled = set(), set()
    for match in re.finditer(pattern, code):
        attrs = source[match.start(1):match.end(1)]
        assert 'cfg_attr' not in attrs, 'cfg_attr modules need explicit audit support'
        gates = [cfg_value(gate, features) for gate in
                 re.findall(r'#\s*\[cfg\((.*?)\)\]', attrs, re.S)]
        active = all(gates) and all(value for end, value in scopes if end > match.start())
        identity = (code.count('\n', 0, match.start()) + 1, match[2])
        declared.add(identity)
        if active:
            enabled.add(identity)
        if match[3] == '{':
            depth = 1
            for end in range(match.end(), len(code)):
                depth += (code[end] == '{') - (code[end] == '}')
                if depth == 0:
                    scopes.append((end, active))
                    break
            else:
                raise AssertionError('unclosed inline module')
    return declared, enabled


def require_module_coverage(declared, enabled):
    # A disabled declaration is a platform alternative when an enabled module
    # of the same name exists in the file; only never-enabled names are flagged.
    enabled_names = {(identity[0], identity[-1]) for identity in enabled}
    missing = {identity for identity in declared - enabled
               if (identity[0], identity[-1]) not in enabled_names}
    assert not missing, 'Test module declarations outside executable sweep shapes:\n' + '\n'.join(
        f'{path}:{line}: mod {name}' for path, line, name in sorted(missing))


def require_coverage(found, covered):
    missing = found - covered
    assert not missing, 'Rust test files outside executable sweep module graphs:\n' + '\n'.join(map(str, sorted(missing)))


def audit(root):
    checks = tomllib.loads((root / 'brokkr.d/checks.toml').read_text())['check']
    members = tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['members']
    found = set()
    covered = set()
    enabled_modules = set()
    for member in members:
        directory = root / member
        manifest = tomllib.loads((directory / 'Cargo.toml').read_text())
        assert manifest['package'].get('autotests') is False, f'{member}: audit expects explicit test targets'
        found.update(path.resolve() for path in (directory / 'tests').rglob('*.rs'))
        for sweep in checks:
            if manifest['package']['name'] not in sweep.get('packages', []):
                continue
            features = set(sweep.get('features', []))
            if not sweep.get('no_default_features', False):
                features.add('default')
            feature_map = manifest.get('features', {})
            while True:
                expanded = features | {value for key in features for value in feature_map.get(key, [])
                                       if '/' not in value and not value.startswith('dep:')}
                if expanded == features:
                    break
                features = expanded
            features.add('musl' if sweep.get('target', '').endswith('-musl') else 'gnu')
            for target in manifest.get('test', []):
                if not set(target.get('required-features', [])).issubset(features):
                    continue
                path = (directory / target.get('path', 'tests/' + target['name'] + '.rs')).resolve()
                graph = reachable([path], features)
                covered.update(graph)
                for file in graph:
                    _, enabled = module_presence(file.read_text(), features)
                    enabled_modules.update((file, line, name) for line, name in enabled)
    require_coverage(found, covered)
    declared_modules = set()
    for file in found:
        declared, _ = module_presence(file.read_text(), set())
        declared_modules.update((file, line, name) for line, name in declared)
    require_module_coverage(declared_modules, enabled_modules)
    return len(found)


if __name__ == '__main__':
    try:
        count = audit(Path(__file__).resolve().parents[1])
        print(f'{count} Rust test files reachable')
        print('test module audit ok')
    except (AssertionError, IndexError, KeyError, OSError) as error:
        sys.exit(str(error))
