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

Old section fragments on this pointer page are not forwarded, and site
search does not index the linked content. Keeping headings or content here
would create a second copy that could drift; use the repository sections.
