# p2pmux metrics

One row per week. Four numbers. Run `sh scripts/metrics.sh` and paste what it prints.

Stars are not on this page and never will be. Neither are clone counts — 1170 clones from 334
uniques is mirrors, package scanners and CI, and reporting it to anyone would be lying.

| Metric | What it proves | Source |
| --- | --- | --- |
| Downloads | Someone **tried** it | `.tar.gz` asset counts across all releases |
| **Stranger issues** | Someone **used** it | Issues opened by anyone who is not `pelazas` |
| Visitors | Top of funnel | 14-day unique visitors. GitHub discards this after 14 days, so it only exists if it is recorded weekly |
| Escalations | Whether v2 exists at all | `docs/ESCALATIONS.md`, one `##` heading per event |

## The decision this page exists to make

**If stranger-opened issues are still zero after ~200 downloads, the problem is the product, not
the marketing.** No amount of Reddit fixes that, and another four weeks of posting would be four
weeks wasted. Check it at the 200 mark before extending the campaign.

The escalation count decides the roadmap independently: **< 2/week kills the agent thesis, 2–5
means ship the inbox and widen to ~10 hand-picked users, > 5 means build hard.**

## Log

| Date | Downloads | Stranger issues | Visitors 14d | Escalations | Notes |
| --- | --- | --- | --- | --- | --- |
| 2026-08-02 | 23 | **0** | 4 | 0 | Baseline. See caveats below. |

### 2026-08-02 — baseline

The starting point, recorded the day `v0.1.2` shipped. Read it pessimistically:

- **4 unique visitors in 14 days against 424 views.** The top of the funnel is empty. Every number
  downstream of it is noise until this moves, and no product change affects it.
- **23 downloads is not 23 people.** 4 on `v0.1.0`, 14 on `v0.1.1`, 5 on `v0.1.2`. `v0.1.0` was
  macOS-only and plausibly all us. **2 of the 5 on `v0.1.2` are the install verification run on
  DigitalOcean on 2026-08-02** — subtract them. Treat the honest figure as under 20, most of it us.
- **Zero stranger-opened issues.** All 6 issues in the repo are the owner's. This is the number
  with the furthest to travel and the only one that proves use rather than curiosity.
- **Zero escalations logged**, because the log did not exist before today. The count starts now,
  and a week of dogfooding is what puts the first entries in it.

Ship state on this date: `v0.1.2` publishes macOS and Linux binaries for both architectures, and
the one-line installer is **verified working end to end on a clean Ubuntu droplet** (24.04, root
and non-root-without-sudo, checksum enforced). The install path is no longer a launch blocker.
