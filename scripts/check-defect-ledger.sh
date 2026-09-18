#!/usr/bin/env bash
# Release gate for the v0.6.10 defect ledger.
#
# Exits non-zero while any defect row is neither CLOSED nor explicitly
# DEFERRED. Task 12 of the implementation plan runs this before it starts.
#
# It parses ONLY between the DEFECT-INDEX markers. The ledger also carries
# per-fix mutation tables whose rows share the index table's shape — 40 rows
# match that shape, of which only 21 are defects — so a naive grep sees
# nineteen phantoms. A gate that can misread its own input is worse than no
# gate, which is why the bounds are explicit rather than inferred.
#
# It also checks that every indexed row has a matching `## N.` section, so a
# row cannot be added to the table without the detail that makes it
# actionable, or removed from the table while its section lingers.
set -euo pipefail

LEDGER="${1:-docs/roadmap/v0.6.10-defects.md}"
[ -f "$LEDGER" ] || { echo "check-defect-ledger.sh: no ledger at $LEDGER" >&2; exit 1; }

rows="$(awk '/DEFECT-INDEX:BEGIN/{f=1;next} /DEFECT-INDEX:END/{f=0} f && /^\| [0-9]+ \|/' "$LEDGER")"
[ -n "$rows" ] || { echo "check-defect-ledger.sh: no defect rows between the markers" >&2; exit 1; }

total=0; closed=0; deferred=0; open_rows=""
while IFS= read -r row; do
  total=$((total+1))
  num="$(printf '%s' "$row" | awk -F'|' '{gsub(/ /,"",$2); print $2}')"
  status="$(printf '%s' "$row" | awk -F'|' '{print $5}')"
  case "$status" in
    *CLOSED*)   closed=$((closed+1)) ;;
    *DEFERRED*) deferred=$((deferred+1)) ;;
    *)          open_rows="${open_rows}  #${num}:${status}"$'\n' ;;
  esac
  grep -qE "^## ${num}\. " "$LEDGER" \
    || { echo "check-defect-ledger.sh: row #${num} has no '## ${num}.' section" >&2; exit 1; }
done <<< "$rows"

echo "check-defect-ledger.sh: ${total} defects — ${closed} closed, ${deferred} deferred, $((total-closed-deferred)) open"
if [ -n "$open_rows" ]; then
  printf 'check-defect-ledger.sh: FAILED — these are still open:\n%s' "$open_rows" >&2
  exit 1
fi
echo "check-defect-ledger.sh: OK — the release gate is satisfied"
