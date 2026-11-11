"""In-memory fixtures for the integration-test module audit.

No filesystem fixtures or host toolchain are needed.
"""
from pathlib import Path
from unittest.mock import patch
from check_test_modules import cfg_value, module_edges, reachable, require_coverage


def main():
    assert cfg_value('all(feature = "commands", not(feature = "test-hooks"))', {'commands'})
    assert not cfg_value('feature = "new-feature"', {'commands'})
    assert cfg_value('any(feature = "a", feature = "b")', {'b'})
    base = Path('/project/tests')
    source = '#[cfg(feature = "hooks")]\n#[path = "helper.rs"]\nmod helper;'
    assert list(module_edges(source, base, set())) == []
    assert list(module_edges(source, base, {'hooks'})) == [(base / 'helper.rs').resolve()]
    assert list(module_edges('// mod missing;\nconst S: &str = "mod missing;";', base, set())) == []
    for source in ('mod inline { mod child; }', '#[cfg_attr(test, path = "x.rs")]\nmod x;'):
        try:
            list(module_edges(source, base, set()))
        except AssertionError:
            pass
        else:
            raise AssertionError(f'unsupported module shape accepted: {source}')
    try:
        cfg_value('unknown', set())
    except AssertionError:
        pass
    else:
        raise AssertionError('unknown cfg accepted')
    root = base / 'all.rs'
    child = base / 'child.rs'
    sources = {root: 'mod child;', child: ''}
    with patch.object(Path, 'is_file', lambda path: path in sources), \
            patch.object(Path, 'read_text', lambda path: sources[path]):
        graph = reachable([root], set())
        assert graph == {root, child}
        require_coverage({root, child}, graph)
        try:
            require_coverage({root, child, base / 'orphan.rs'}, graph)
        except AssertionError:
            pass
        else:
            raise AssertionError('orphan file accepted')
        # A #[path] in a non-mod.rs file resolves beside that file, while a
        # default module name resolves under its stem directory.
        shared = base / 'shared.rs'
        nested = base / 'child' / 'nested.rs'
        sources[child] = '#[path = "shared.rs"]\nmod shared;\nmod nested;'
        sources[shared] = ''
        sources[nested] = ''
        assert reachable([root], set()) == {root, child, shared, nested}
        del sources[shared], sources[nested]
        sources[child] = '#![cfg(feature = "never-enabled")]'
        assert child not in reachable([root], set())
        assert child in reachable([root], {'never-enabled'})
        del sources[child]
        try:
            reachable([root], set())
        except AssertionError:
            pass
        else:
            raise AssertionError('missing declared module accepted')
    print('test module fixtures ok')


if __name__ == '__main__':
    main()
