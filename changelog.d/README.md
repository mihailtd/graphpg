# Changelog fragments

A pull request adds its CHANGELOG entry as a file of its own here, instead of editing
`CHANGELOG.md`: new files never conflict, while every pull request editing the top of
`CHANGELOG.md` conflicted with every other.

- One file per change, named after the branch or the issue: `changelog.d/rate-limits.md`.
- Its content is the bullet as it will appear under the release, starting with `- `.
- At release, `changelog.d/collect.sh <version>` moves every fragment into a new section of
  `CHANGELOG.md` (oldest first) and deletes them.
