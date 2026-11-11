"""Keep the literal getrandom identifier in the scratch entropy adapter.

Lexical ownership only: aliases imported from elsewhere, other entropy APIs
and macro-generated identifiers require review. Comments and literals cannot
supply a call or an exemption. No production or test file is exempt wholesale.
"""
from pathlib import Path
import re
import sys

from rust_source_shapes import blank_comments_and_literals, function_body

OWNER = 'crates/lib/src/scratch.rs'
# must_scan sentinels pin every selected workspace source tree.
MUST_SCAN = (OWNER, 'crates/lib/src/read/reader.rs',
             'crates/cli/src/lib.rs', 'crates/harness/src/main.rs')
PATTERN = re.compile(r'\bgetrandom\b')


def ownership_errors(sources):
    errors = []
    for path, source in sorted(sources.items()):
        code = blank_comments_and_literals(source)
        matches = list(PATTERN.finditer(code))
        if path == OWNER:
            body = function_body(source, 'random_u128')
            if body is None or len(PATTERN.findall(body)) != 1:
                errors.append(f'{path}: random_u128 must own one getrandom acquisition')
            if len(matches) != 1:
                errors.append(f'{path}: getrandom belongs only in random_u128')
        elif matches:
            for match in matches:
                line = code.count('\n', 0, match.start()) + 1
                errors.append(f'{path}:{line}: getrandom belongs in scratch::random_u128')
    if OWNER not in sources:
        errors.append(f'{OWNER}: entropy owner missing')
    return errors


def audit(root):
    for sentinel in MUST_SCAN:
        if not (root / sentinel).is_file():
            raise ValueError(f'must_scan sentinel missing: {sentinel}')
    sources = {str(path.relative_to(root)): path.read_text()
               for member in ('lib', 'cli', 'harness')
               for path in (root / 'crates' / member / 'src').rglob('*.rs')}
    errors = ownership_errors(sources)
    if errors:
        raise ValueError('\n'.join(errors))


if __name__ == '__main__':
    try:
        audit(Path(__file__).resolve().parents[1])
    except (OSError, ValueError) as error:
        sys.exit(str(error))
    print('entropy owner ok')
