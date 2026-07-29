# Stage 1 analysis prompt

Run this in Claude Code from `scripts/launch-research/`, after `winners.json` exists.
Paste it as a single prompt.

---

Read `winners.json`. Each entry is a high-engagement X post with `_handle` and
`_followers`, plus a performance measure that depends on which collector produced it:

- From `harvest_top.py`: `_per_1k` — weighted engagement per thousand followers, and
  `_query`, the search angle that surfaced it. Every post here already won its field;
  `_per_1k` says which ones punched above their author's audience size.
- From `fetch_winners.py`: `_ratio` — how many times the author's own median views the
  post hit — and `_quality`, weighted engagement per view.

Use whichever is present. Below, "score" means `_per_1k` or `_ratio`.

Produce `SWIPE_FILE.md`. Do not summarise the posts — decompose them. For every post,
extract:

1. **First line, verbatim.** The scroll-stopper is almost always the first 8 words.
2. **Opening move** — classify it: personal result claim, contrarian take, number,
   confession/failure, question, direct product statement, or announcement.
3. **Specificity** — does the hook contain a concrete number, name, or timeframe? Mark
   yes/no. Compare the yes-rate among the top decile of score against the bottom.
4. **Media** — video, image, text-only, or link. Inspect the raw fields; if media data
   is absent from the JSON, open a sample of URLs and record it by hand.
5. **Ask** — what the reader is told to do: nothing, reply a keyword, click, follow,
   repost.
6. **Length** — characters, and whether it's a single post or the head of a thread.

Then write the analysis sections:

- **Ranked opening moves.** Which classifications appear most among high-scoring posts,
  with counts. Report this as a table, and say plainly which categories are too thin to
  conclude anything from.
- **Hook skeletons.** Extract 8-12 reusable structural patterns with the specifics
  blanked, e.g. "I [did X] in [timeframe] and [surprising result]". Patterns, not
  sentences.
- **Reach versus conversation.** Compare posts whose likes dominate against posts whose
  replies and bookmarks dominate: the first got reach, the second got conversation. Name which opening moves produce each — the second kind is what a
  10-qualified-leads launch actually wants.
- **Anti-patterns.** Common moves that appear in the sample but cluster at the bottom.
- **Applicability to p2pmux.** For each top skeleton, one line: does it survive being
  written about a peer-to-peer terminal where each pane runs on its own machine under
  its own credentials? Some hooks only work for consumer products; mark those dead.

Rules:

- Never invent a post, a number, or a handle. Every claim traces to an entry in the JSON.
- Where the sample is too small to support a pattern, say so instead of asserting it.
- The output is for studying why things worked. Do not draft launch copy in this file;
  reusing someone's sentences is both obvious to their audience and useless to yours.
