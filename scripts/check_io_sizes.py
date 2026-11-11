"""Reject literal arithmetic copies of the shared transfer size.

This lexical check evaluates maximal integer-only expressions, not arbitrary
Rust const evaluation. Named aliases, casts and macro-generated arithmetic
remain outside its promise. Scan/cache budgets are evaluated as a whole, so
256 * 1024 * 1024 is not mistaken for a transfer buffer.
"""
import ast
import operator
from pathlib import Path
import re
import sys

from rust_source_shapes import blank_comments_and_literals

OPS = {ast.Add: operator.add, ast.Sub: operator.sub, ast.Mult: operator.mul,
       ast.LShift: operator.lshift, ast.RShift: operator.rshift,
       ast.BitOr: operator.or_, ast.BitAnd: operator.and_, ast.BitXor: operator.xor}
TOKEN = re.compile(r'0[xX][\da-fA-F_]+(?:usize|u\d+)?|0[bB][01_]+(?:usize|u\d+)?|0[oO][0-7_]+(?:usize|u\d+)?|\d[\d_]*(?:usize|u\d+)?|<<|>>|[()+*^&|\-]|[A-Za-z_]\w*|[^\s]')
NUMBER = re.compile(r'(0[xX][\da-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|\d[\d_]*)(?:usize|u\d+)?$')


def integer_value(expression):
    def evaluate(node):
        if isinstance(node, ast.Constant) and type(node.value) is int:
            return node.value
        if isinstance(node, ast.BinOp) and type(node.op) in OPS:
            left, right = evaluate(node.left), evaluate(node.right)
            if isinstance(node.op, (ast.LShift, ast.RShift)) and not 0 <= right <= 64:
                raise ValueError('shift outside audited integer subset')
            return OPS[type(node.op)](left, right)
        raise ValueError('not integer arithmetic')
    return evaluate(ast.parse(expression, mode='eval').body)


def duplicate_lines(source, size):
    code = blank_comments_and_literals(source)
    run = []

    def check():
        if not run:
            return None
        expression = ''.join(value for _, value in run)
        # Parentheses belonging to an enclosing call or expression can be
        # unbalanced at a lexical boundary. Remove only unmatched outer ones.
        while expression.startswith('(') and expression.count('(') > expression.count(')'):
            expression = expression[1:]
        while expression.endswith(')') and expression.count(')') > expression.count('('):
            expression = expression[:-1]
        try:
            if integer_value(expression) == size:
                return code.count('\n', 0, run[0][0]) + 1
        except (SyntaxError, ValueError, TypeError):
            pass
        return None

    for token in TOKEN.finditer(code):
        value = token[0]
        number = NUMBER.fullmatch(value)
        if number or value in ('(', ')', '+', '-', '*', '<<', '>>', '|', '&', '^'):
            run.append((token.start(), number[1].replace('_', '') if number else value))
        else:
            line = check()
            if line is not None:
                yield line
            run = []
    line = check()
    if line is not None:
        yield line


def audit(root):
    owner = root / 'crates/lib/src/tunables.rs'
    code = blank_comments_and_literals(owner.read_text())
    declarations = re.findall(r'const\s+IO_BUF_BYTES\s*:\s*usize\s*=\s*([^;]+);', code)
    assert len(declarations) == 1, 'expected one IO_BUF_BYTES owner'
    size = integer_value(declarations[0].replace('_', ''))
    assert size > 0
    source = root / 'crates/lib/src'
    sentinels = ['read/blob.rs', 'read/reader.rs', 'write/writer.rs',
                 'write/direct_writer.rs', 'write/uring_writer.rs']
    for path in sentinels:
        assert (source / path).is_file(), f'I/O corpus sentinel missing: {path}'
    paths = sorted(path for directory in ('read', 'write')
                   for path in (source / directory).rglob('*.rs'))
    assert paths
    errors = [f'{path.relative_to(root)}:{line}: use IO_BUF_BYTES for the transfer size'
              for path in paths for line in duplicate_lines(path.read_text(), size)]
    assert not errors, '\n'.join(errors)
    return len(paths)


if __name__ == '__main__':
    try:
        count = audit(Path(__file__).resolve().parents[1])
        print(f'{count} I/O source files scanned')
        print('io size audit ok')
    except (AssertionError, OSError, ValueError, SyntaxError) as error:
        sys.exit(str(error))
