"""In-memory fixtures for lexical entropy ownership; no Rust build."""
from check_entropy_owner import OWNER, ownership_errors


def main():
    good = 'fn random_u128() { unsafe { libc::getrandom(p, n, 0) }; }'
    assert not ownership_errors({OWNER: good})
    assert not ownership_errors({OWNER: good, 'other.rs': '''
        // libc::getrandom(p, n, 0)
        /* nested /* getrandom */ comment */
        const TEXT: &str = r###"getrandom"###;
    '''})
    for bad in (
        'fn random_u128() {}',
        good + '\nfn stray() { libc::getrandom(p,n,0); }',
        'fn random_u128() {} fn stray() { libc::getrandom(p,n,0); }',
        good + '\nfn random_u128() {}',
        'fn random_u128() { let s = "getrandom"; }',
    ):
        assert ownership_errors({OWNER: bad}), bad
    for bad in ('libc::getrandom(p,n,0);', 'use libc::getrandom as entropy;',
                'libc::\n getrandom(p,n,0);'):
        assert ownership_errors({OWNER: good, 'other.rs': bad}), bad
    assert ownership_errors({})
    print('entropy owner fixtures ok')


if __name__ == '__main__':
    main()
