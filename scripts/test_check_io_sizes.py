"""In-memory positive and negative fixtures for the transfer-size audit."""
from check_io_sizes import duplicate_lines


def main():
    for expression in ('256 * 1024', '262_144usize', '1 << 18', '128 * 2048',
                       '0x40000', '0b1000000000000000000', '(128 * 1024) * 2'):
        assert list(duplicate_lines(f'let size = {expression};', 262144)), expression
    for source in ('// 256 * 1024\nlet x = IO_BUF_BYTES;',
                   'let text = "262144";', 'let text = r#"1 << 18"#;',
                   'let budget = 256 * 1024 * 1024;', 'let x = 1262144;',
                   'let x = 0x400000;', '/* nested /* 262144 */ comment */'):
        assert not list(duplicate_lines(source, 262144)), source
    assert list(duplicate_lines('let size = 1 << 17;', 131072))
    print('io size fixtures ok')


if __name__ == '__main__':
    main()
