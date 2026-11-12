"""In-memory fixtures for the CLI tests' library-surface audit."""
from check_test_imports import Unsupported, violations


def refused(source):
    return [path for _, path in violations(source)]


def main():
    passing = (
        'use pbfhogg::writer::{Compression, PbfWriter};',
        'use pbfhogg::{BlobDecode, BlobReader, Element, MemberId};',
        'use pbfhogg::writer::PbfWriter as Writer;',
        'use pbfhogg::test_hooks::PublishFault::{self, *};',
        'let w = pbfhogg::writer::PbfWriter::new(f, pbfhogg::writer::Compression::None);',
        'let t = ::pbfhogg::TEST_CHILD_TIMEOUT;',
        'use std::path::Path;\nuse crate::invoker::CliInvoker;',
        'let x = pbfhogg_cli::cli_command();',
        '// pbfhogg::cat::run in a comment\nlet x = 1;',
        'let s = "pbfhogg::cat::run";',
        'let x = pbfhogg::cat::run(); // library-deliberate: a reason',
        'use pbfhogg::{\n    writer::PbfWriter,\n    cat::run, // library-deliberate: a reason\n};',
        'use pbfhogg::{ // library-deliberate: a reason\n    cat::run,\n};',
        'let n = self.pbfhogg::<u8>;',
        '    // library-deliberate: a reason\n    let x = pbfhogg::cat::run();',
        'let p = pbfhogg::Info::new();',
    )
    for source in passing:
        assert refused(source) == [], (source, refused(source))

    failing = {
        'use pbfhogg::tags_count::tags_count;': ['pbfhogg::tags_count::tags_count'],
        'use pbfhogg::{\n    writer::PbfWriter,\n    commands::parse_rfc3339_utc,\n};':
            ['pbfhogg::commands::parse_rfc3339_utc'],
        'use {\n    std::path::Path,\n    pbfhogg::commands::parse_rfc3339_utc,\n};':
            ['pbfhogg::commands::parse_rfc3339_utc'],
        'use { pbfhogg::{ writer::PbfWriter, cat::run } };': ['pbfhogg::cat::run'],
        'use pbfhogg /* note */ :: commands::parse;': ['pbfhogg::commands::parse'],
        'use r#pbfhogg::r#cat::run;': ['pbfhogg::cat::run'],
        'let p = pbfhogg::Parallelism::from_jobs(1);': ['pbfhogg::Parallelism::from_jobs'],
        'use pbfhogg::writer as writer;': ['pbfhogg::writer as ...'],
        'use pbfhogg::writer::*;': ['pbfhogg::writer::*'],
        'use pbfhogg::writer::{self, PbfWriter};': ['pbfhogg::writer'],
        'use pbfhogg::block_builder;': ['pbfhogg::block_builder'],
        'use pbfhogg as p;': ['pbfhogg as ...'],
        'use pbfhogg::*;': ['pbfhogg::*'],
        'extern crate pbfhogg;': ['extern crate pbfhogg'],
        'let r = pbfhogg::cat(); let t = "// library-deliberate: example";': ['pbfhogg::cat'],
        'let r = pbfhogg::cat(); /* // library-deliberate: example */': ['pbfhogg::cat'],
        'let r = pbfhogg::cat(); // library-deliberate:': ['pbfhogg::cat'],
        'let r = pbfhogg::writer::Other::new();': ['pbfhogg::writer::Other::new'],
        'let r = pbfhogg::BlobReaderish::new();': ['pbfhogg::BlobReaderish::new'],
        'let a = 1; // library-deliberate: a reason\nlet r = pbfhogg::cat();': ['pbfhogg::cat'],
        '// library-deliberate: a reason\n\nlet r = pbfhogg::cat();': ['pbfhogg::cat'],
    }
    for source, expected in failing.items():
        assert refused(source) == expected, (source, refused(source))

    for source in ('use pbfhogg::{writer::PbfWriter', 'use pbfhogg::writer::PbfWriter as;',
                   'use pbfhogg::self::writer;', 'use pbfhogg::{a b};'):
        try:
            violations(source)
        except Unsupported:
            continue
        raise AssertionError(f'unsupported shape passed: {source!r}')
    print('test import fixtures ok')


if __name__ == '__main__':
    main()
