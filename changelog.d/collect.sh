#!/usr/bin/env bash
# Moves the fragments of changelog.d/ into a `## <version> - <date>` section of CHANGELOG.md,
# above the previous releases, oldest fragment first, and deletes them.
set -euo pipefail
version=${1:?usage: changelog.d/collect.sh <version>}
cd "$(dirname "$0")/.."
mapfile -t fragments < <(git log --diff-filter=A --format= --name-only --reverse -- 'changelog.d/*.md' | grep -v README | while read -r f; do [ -f "$f" ] && echo "$f"; done | awk '!seen[$0]++')
[ ${#fragments[@]} -gt 0 ] || grep -q '^## Unreleased' CHANGELOG.md || { echo "no fragments in changelog.d/" >&2; exit 1; }
section=$(mktemp)
{
  echo "## $version - $(date +%Y-%m-%d)"; echo
  # Entries still under a `## Unreleased` heading come first, then the fragments
  awk '/^## Unreleased/{f=1;next} /^## /{f=0} f' CHANGELOG.md | sed '/^$/d'
  for f in "${fragments[@]}"; do cat "$f"; echo; done | sed '/^$/d'
  echo
} > "$section"
# Drop the `## Unreleased` section, and put the new one above the latest release
awk -v file="$section" '
  /^## Unreleased/ {skip=1; next}
  /^## / {skip=0; if (!done) {while ((getline l < file) > 0) print l; done=1}}
  !skip {print}
' CHANGELOG.md > CHANGELOG.md.new
mv CHANGELOG.md.new CHANGELOG.md
[ ${#fragments[@]} -eq 0 ] || git rm -q "${fragments[@]}"
echo "moved ${#fragments[@]} fragments into CHANGELOG.md ($version)"
