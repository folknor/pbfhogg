# getparents

Earlier getparents cells (planet and europe walls, the projection change, the
arm dispatch) are in `classify.md`, `projections.md`, `cross-command.md`,
`read-pipeline.md`, `write-pipeline.md` and `memory.md`.

## `--add-self` selects node and way blobs by claimed ID range (bygg, 2026-11-26)

Baseline `dfd36268` (`brokkr bench --commit`), variant the same tree with the
change uncommitted; `parent-select --add-self yes`, `--iterations 1` cells
interleaved baseline first. Germany uses its configured `getparents_ids`
(`n442734 n442740 w481211268`), europe the built-in Denmark IDs.

| Dataset (arm) | Baseline cells | Variant cells | Blobs decoded / skipped, baseline -> variant |
|---|---|---|---|
| germany (walker) | 2.3 s `31e035e2` (cold first cell), 1.0 s `498fb38e` | 665 ms `1ec8236c`, 669 ms `5988f3d1` | scheduled 62,461 / 0 -> 8,808 / 53,653 |
| europe (full scan) | 10.8 s `fb037e8d`, 10.5 s `23ac5a48` | 10.9 s `0c5f7301`, 10.4 s `7c6dff52` | 464,446 skipped before decompression in the variant |

Germany: a node query with `--add-self` used to schedule every node blob; it
now reads only the node blob whose claimed range holds the queried nodes, and
the wall falls to the plain query's (plain `parent-select`, no add-self:
657 ms `fef8b5ef` vs 661 ms `6bda96de`, unchanged). Europe: the full scan arm
streams every byte whatever it decodes (34 GB disk read in both arms), so
the wall does not move; the skipped decompression shows as CPU, 21.0 average
cores down to 14.9 over `GETPARENTS_DECODE`. Not a regression on either arm.
