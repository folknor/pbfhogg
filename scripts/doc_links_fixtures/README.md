# Document link compatibility corpus

`anchors.json` contains expected heading-ID projections, derived by reading
the installed source, not captured HTML and not output from the checker.
No renderer, test or build was executed to obtain these expectations.
The Python self-test consumes this committed corpus without Node installed.

## VitePress provenance

The root package and pnpm lock pin VitePress 2.0.0-alpha.17. The installed
package is under `node_modules/.pnpm/`, in the directory whose name begins
`vitepress@2.0.0-alpha.17_`. Source paths below are relative to that package's
`node_modules/vitepress/` directory.

- `dist/node/chunk-CpMTk0EA.js`, SHA-256
  `e986f1920277caf64656389d6460365b809112a273a7792a8308eb5951a86b96`:
  lines 9020-9028 define the slug transformations. Reading these in order
  gives punctuation collapse, digit prefixing, accent decomposition and
  underscore conversion in cases 1-6.
- Lines 29246-29255 install attributes before anchors and concatenate inline
  token contents except HTML and emoji. Text inside emphasis and links,
  literal code contents and image alt content therefore feed cases 7-9.
  Unlike HTML tags, angle brackets inside code are literal token content.
- The bundled markdown-it-anchor code at line 16287 reserves every emitted
  ID and searches successive suffixes until unused. Explicit heading IDs
  participate in that reservation. Cases 10-11 exercise both paths.
  Duplicate explicit IDs throw in VitePress; they are outside this corpus.
- The bundled `heading`, `lheading` and `fence` rules supply the simple ATX,
  Setext and nested fence-run inputs in cases 1, 2 and 12.

These are hand-derived projections of the ID attribute only. They establish
agreement for these inputs, not general Markdown or renderer equivalence.
On a VitePress upgrade, re-read the installed implementations and re-derive
the corpus, rather than updating expectations from the checker.

## GitHub compatibility contract

The independent reference is GitHub's [section-link documentation](https://docs.github.com/en/get-started/writing-on-github/getting-started-with-writing-and-formatting-on-github/basic-writing-and-formatting-syntax#section-links),
consulted 2026-11-11. This corpus applies its documented lowercase, space,
punctuation, formatting and repeated-heading rules to small authored inputs.
Its custom-anchor section says HTML anchors do not affect automatic numbering.
The last case records that VitePress attribute syntax is ordinary heading
text on GitHub. This is a documentation-based contract, not a capture from
GitHub's private renderer and not a claim of Unicode slugger equivalence.

## Sidebar boundary and remaining limitations

The self-test also checks literal sidebar objects and arrays, nested items,
comments, quoted keys, fragments and exclusion of navigation links and section
keys. These are authored checker-contract cases. VitePress consumes JavaScript
objects; it does not parse TypeScript with this checker's token recognizer.
Its `dist/client/theme-default/support/sidebar.js` implements `getSidebar`,
`getFlatSideBarLinks` and `addBase`. Its SHA-256 is
`f8be1d829583fc7c165539b1ccb4a88c9f414c97634e79e61fc724d0caae8129`.
The checker's coverage means a page is named somewhere, not that each route
selects a sidebar containing it. Computed configs, spread properties, inherited
sidebar bases and per-route selection are not renderer-verified here.

Inline entities, escapes, reference links, nested inline syntax, emoji,
multiline inline token boundaries, HTML blocks, container headings and
indented code are outside this corpus. In particular VitePress's
`restoreEntities` and `getTokensText` are not equivalent to plain rendered
text extraction. The scanner must not be advertised as a full parser.
