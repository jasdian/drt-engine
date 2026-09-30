#!/usr/bin/env sh
# Drift gate between docs/extending.md's invariants checklist and the
# drt-no-mistakes skill: the ten bold phrases of the checklist must appear
# verbatim as the skill's `## N. <phrase>` headings, and the skill must have
# exactly ten such headings. The near-zero analog of AXI's docs-check CI.
#
# Usage: scripts/skill-check.sh [extending.md] [SKILL.md]   (paths override for tests)
set -u
cd "$(dirname "$0")/.."
DOC="${1:-docs/extending.md}"
SKILL="${2:-.agents/skills/drt-no-mistakes/SKILL.md}"
fail=0

# "N|phrase" pairs: first bold span of each numbered checklist item, trailing period stripped.
pairs=$(sed -n '/^## Invariants checklist/,/^## /p' "$DOC" \
  | grep -E '^[0-9]+\. \*\*' \
  | sed -E 's/^([0-9]+)\. \*\*([^*]+)\*\*.*/\1|\2/' \
  | sed -E 's/\.$//')

count=$(printf '%s\n' "$pairs" | grep -c '|' || true)
if [ "$count" -ne 10 ]; then
  echo "skill-check: expected 10 checklist items in $DOC, found $count"
  fail=1
fi

while IFS='|' read -r n phrase; do
  [ -n "$n" ] || continue
  if ! grep -qF "## $n. $phrase" "$SKILL"; then
    echo "skill-check: heading missing or drifted in $SKILL: '## $n. $phrase'"
    fail=1
  fi
done <<EOF
$pairs
EOF

headings=$(grep -cE '^## [0-9]+\. ' "$SKILL" || true)
if [ "$headings" -ne 10 ]; then
  echo "skill-check: expected 10 numbered headings in $SKILL, found $headings"
  fail=1
fi

# The worked example is committed as task #10 — its doc blocks must not drift
# from the committed tree branch and task manifest.
TREE=bench/general-tree.json
TASK=bench/tasks/repo_topics.toml
doc_branch=$(awk '/\*\*Step 1/{s=1} s&&/^```json$/{f=1;next} f&&/^```$/{exit} f' "$DOC")
if [ -n "$doc_branch" ] && [ -f "$TREE" ]; then
  if ! printf '%s' "$doc_branch" | jq -S . > /tmp/skill-check-doc.json 2>/dev/null; then
    echo "skill-check: worked-example branch JSON in $DOC does not parse"
    fail=1
  elif ! jq -S '.root.candidates[8]' "$TREE" > /tmp/skill-check-tree.json \
      || ! cmp -s /tmp/skill-check-doc.json /tmp/skill-check-tree.json; then
    echo "skill-check: worked-example branch in $DOC drifted from $TREE root.candidates[8]"
    fail=1
  fi
fi
doc_toml=$(awk '/\*\*Step 2/{s=1} s&&/^```toml$/{f=1;next} f&&/^```$/{exit} f' "$DOC")
if [ -n "$doc_toml" ] && [ -f "$TASK" ]; then
  if ! printf '%s\n' "$doc_toml" | python3 -c '
import sys, tomllib
doc = tomllib.loads(sys.stdin.read())
real = tomllib.load(open(sys.argv[1], "rb"))
sys.exit(0 if doc == real else 1)
' "$TASK" 2>/dev/null; then
    echo "skill-check: worked-example task TOML in $DOC drifted from $TASK"
    fail=1
  fi
fi

if [ "$fail" -eq 0 ]; then
  echo "skill-check: OK — 10 invariant headings in sync; worked example matches committed task #10"
fi
exit "$fail"
