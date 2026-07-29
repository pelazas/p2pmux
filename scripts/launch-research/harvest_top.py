#!/usr/bin/env python3
"""Collect winning posts directly, for when credits are scarce.

Usage:
    export TWITTERAPI_KEY=...
    python3 harvest_top.py [max_requests]

fetch_winners.py implements the textbook method: pull an account's whole recent
timeline, then keep the posts that beat its own median. That method is correct but
wasteful — it spends roughly twelve posts of quota for every winner it keeps.

X's own `queryType=Top` already ranks by engagement, so every post it returns is
someone's winner. On a tight budget that is ten times more swipe file per credit. What
you give up is the per-account baseline: you cannot say a post beat its author's median,
only that it beat the field. `_per_1k` below is the stand-in — engagement per thousand
followers, which is what "punched above their weight" means when you have no baseline.

Budget: each request returns up to 20 posts and costs 20 x 15 = 300 credits.
Check your balance first:
    curl -s https://api.twitterapi.io/oapi/my/info -H "x-api-key: $TWITTERAPI_KEY"
"""

import json
import os
import sys

from xapi import RateLimited, call, field

SEARCH = "/twitter/tweet/advanced_search"
CREDITS_PER_REQUEST = 300
MIN_FAVES = 25

# One page of Top results per query. Ten different angles beats ten pages of one angle:
# page two of any query is already down the long tail, while a fresh query opens a
# fresh set of winners.
QUERIES = [
    ('multiplexer', 'terminal multiplexer OR tmux OR zellij min_faves:%d -filter:replies'),
    ('agents-terminal', '"claude code" (terminal OR agents OR parallel) min_faves:%d -filter:replies'),
    ('coding-agent', '"coding agent" OR "coding agents" min_faves:%d -filter:replies'),
    ('rust-tui', '(rust OR zig) (TUI OR terminal OR ratatui) min_faves:%d -filter:replies'),
    ('shipped', '("just shipped" OR "just launched" OR "launching today") (CLI OR terminal OR devtool OR agent) min_faves:%d -filter:replies'),
    ('terminal-emulators', 'ghostty OR wezterm OR alacritty OR warpdotdev min_faves:%d -filter:replies'),
    ('dev-setup', '"terminal setup" OR "dev setup" OR dotfiles min_faves:%d -filter:replies'),
    ('remote-pain', '(ssh OR "remote dev" OR "dev environment") (painful OR annoying OR broken) min_faves:%d -filter:replies'),
    ('p2p-devtool', '("peer to peer" OR p2p OR "self-hosted") (developer OR devtool OR CLI) min_faves:%d -filter:replies'),
    ('showhn', '"show hn" (terminal OR CLI OR agent OR rust) min_faves:%d -filter:replies'),
]


def per_1k(post, followers):
    """Weighted engagement per 1k followers.

    Replies, reposts and bookmarks are weighted above likes because those are the
    signals that push a post beyond its author's own followers. Dividing by audience
    size is what lets a 9k-follower account's hook outrank a 300k account's.
    """
    weighted = ((post.get("replyCount") or 0) + (post.get("retweetCount") or 0)
                + (post.get("bookmarkCount") or 0) + (post.get("likeCount") or 0) * 0.25)
    return round(weighted / max(followers, 1) * 1000, 2)


def main():
    key = os.environ.get("TWITTERAPI_KEY")
    if not key:
        sys.exit("Set TWITTERAPI_KEY first: export TWITTERAPI_KEY=...")
    budget = int(sys.argv[1]) if len(sys.argv) > 1 else len(QUERIES)
    print(f"Budget: {budget} requests ~= {budget * CREDITS_PER_REQUEST:,} credits "
          f"(~${budget * CREDITS_PER_REQUEST / 100000:.2f})\n")

    harvested, seen, spent = [], set(), 0

    for label, template in QUERIES:
        if spent >= budget:
            print(f"Budget reached; {len(QUERIES) - spent} queries not run.")
            break
        q = template % MIN_FAVES
        print(f"[{spent + 1}/{budget}] {label:18}", end=" ", flush=True)
        try:
            data = call(SEARCH, {"query": q, "queryType": "Top", "cursor": ""}, key)
        except RateLimited as e:
            print(f"RATE LIMITED ({e})")
            spent += 1
            continue
        except Exception as e:                      # noqa: BLE001 - report and continue
            print(f"FAILED ({e})")
            spent += 1
            continue
        spent += 1
        posts = data.get("tweets") or []
        added = 0
        for p in posts:
            if p.get("id") in seen:
                continue
            seen.add(p.get("id"))
            a = p.get("author") or {}
            followers = field(a, "followers", "followersCount", default=0) or 0
            p["_query"] = label
            p["_handle"] = field(a, "userName", "screen_name", default="?")
            p["_followers"] = followers
            p["_per_1k"] = per_1k(p, followers)
            harvested.append(p)
            added += 1
        print(f"{len(posts):3} posts, {added:3} new")

    harvested.sort(key=lambda p: p["_per_1k"], reverse=True)

    with open("winners.json", "w") as fh:
        json.dump(harvested, fh, indent=2, ensure_ascii=False)

    with open("winners.md", "w") as fh:
        fh.write(f"# Swipe file — {len(harvested)} top posts across "
                 f"{len(set(p['_handle'] for p in harvested))} accounts\n\n")
        fh.write("Collected via X's own Top ranking, so every post here already beat "
                 "its field. Ranked by weighted engagement per 1k followers.\n\n")
        for p in harvested:
            fh.write(f"## @{p['_handle']} — {p['_per_1k']} per 1k "
                     f"({p['_followers']:,} followers) [{p['_query']}]\n\n")
            fh.write(f"- likes {p.get('likeCount', 0):,} "
                     f"| replies {p.get('replyCount', 0):,} "
                     f"| reposts {p.get('retweetCount', 0):,} "
                     f"| bookmarks {p.get('bookmarkCount', 0):,}\n")
            fh.write(f"- {p.get('url', '')}\n\n")
            fh.write("```\n" + (p.get("text") or "").strip() + "\n```\n\n")

    print(f"\n{len(harvested)} posts -> winners.json, winners.md")
    print(f"Spent ~{spent * CREDITS_PER_REQUEST:,} credits "
          f"(~${spent * CREDITS_PER_REQUEST / 100000:.2f})")


if __name__ == "__main__":
    main()
