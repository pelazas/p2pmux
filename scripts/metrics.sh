#!/bin/sh
# Prints this week's metrics row.
#
# Four numbers, and the reason each one is here rather than the obvious ones:
#
#   downloads    someone TRIED it. Release asset counts, .tar.gz only — the .sha256
#                files track them exactly and would double every number.
#   issues       someone USED it, and cared enough to come back. Counts issues and
#                discussions opened by anyone who is not the repo owner. This is the
#                number the roadmap turns on.
#   visitors     top of funnel. GitHub only keeps 14 days, so this must be recorded
#                weekly or it is gone.
#   escalations  whether v2 exists at all. Not derivable from GitHub. It is `n/a` when
#                no local escalation file is available.
#
# Deliberately absent: stars (vanity) and clone counts. 1170 clones from 334 uniques is
# mirrors, package scanners and CI, not people. Reporting it to anyone would be lying.
#
# Usage:  sh scripts/metrics.sh
set -eu

REPO="${P2PMUX_METRICS_REPO:-pelazas/p2pmux}"
OWNER="${REPO%%/*}"

command -v gh >/dev/null 2>&1 || { echo "metrics.sh: needs the gh CLI" >&2; exit 1; }

downloads=0
for tag in $(gh release list --repo "$REPO" --json tagName --jq '.[].tagName'); do
  n=$(gh release view "$tag" --repo "$REPO" --json assets \
        --jq '[.assets[] | select(.name | endswith(".tar.gz")) | .downloadCount] | add // 0')
  downloads=$((downloads + n))
done

# Stranger-opened only. Anything the owner filed is a self-report, not a signal.
issues=$(gh issue list --repo "$REPO" --state all --limit 200 --json author \
           --jq "[.[] | select(.author.login != \"$OWNER\")] | length")

views=$(gh api "repos/$REPO/traffic/views" --jq '.uniques')

esc_file="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/ESCALATIONS.md"
if [ -f "$esc_file" ]; then
  # One escalation per `## ` heading, counting only below the entries marker — the
  # copy-paste template above it also starts with `## ` and would report a phantom event.
  escalations=$(awk '/^<!-- entries below/ { on = 1; next } on && /^## / { n++ } END { print n + 0 }' "$esc_file")
else
  escalations="n/a"
fi

printf '| %s | %s | **%s** | %s | %s |  |\n' \
  "$(date -u +%Y-%m-%d)" "$downloads" "$issues" "$views" "$escalations"
