# Correctness notes

Parser and encoder edge cases and data representation limits that pbfhogg
accepts by design: how it treats malformed or unusual input, what the library
API returns for it, and which sidecar claims (`indexdata`, `tagdata`, the sort
flag) it believes rather than verifies.

They are kept in one place, in the repository:
[CORRECTNESS.md](https://github.com/folknor/pbfhogg/blob/main/CORRECTNESS.md).
This site does not carry a copy, so there is no second version to fall out of
date.

For intentional behavioral differences from osmium, see
[Deviations](../cli/deviations.md).
