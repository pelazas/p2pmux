#!/usr/bin/env python3
"""Discover candidate accounts for the study list by searching X, not by recalling handles.

Usage:
    export TWITTERAPI_KEY=...
    python3 discover_accounts.py

Writes candidates.md (ranked, with evidence) and candidates.txt (paste-ready handles).

Runtime is throttle-bound: ~5s per request on the free tier, so ~30 requests is about
three minutes. Cost is about $0.09.

This exists because a hand-written list is a list of accounts you already know, and the
accounts you already know are the ones whose hooks you have already absorbed.
"""

import os
import sys
from collections import defaultdict

from xapi import RateLimited, call, field

SEARCH = "/twitter/tweet/advanced_search"
PAGES_PER_QUERY = 3          # 20 posts per page
MIN_FOLLOWERS = 2_000        # below this there is no signal, just noise
MAX_FOLLOWERS = 400_000      # above this the wins need reach you do not have
MIN_FAVES = 20

# Words that mark a post as belonging to your niche rather than merely matching a
# search keyword. Deliberately excludes bare "p2p", "nat" and "open source": a Gears
# of War post listing "Steam P2P" as a feature scored 2/2 on-topic on an earlier run.
# The search query already supplies the topic, so this set exists to confirm the post
# is about developer tooling, not to restate the query.
NICHE = {
    "terminal", "tmux", "zellij", "multiplexer", "cli", "shell", "ssh", "tui",
    "ratatui", "neovim", "vim", "dotfiles", "pane", "pty", "stdout",
    "claude code", "codex", "coding agent", "devtool", "developer tool",
    "repo", "git ", "compiler", "localhost", "daemon", "self-host",
}

# Categories that keep surfacing on these queries and are never your buyer.
NOISE = {
    "gears of war", "steam", "gaming", "airdrop", "nft", "token", "crypto",
    "trading", "presale", "web3", "memecoin", "giveaway",
}

QUERIES = [
    'terminal multiplexer OR tmux OR zellij min_faves:%d -filter:replies' % MIN_FAVES,
    '"claude code" (agent OR agents OR terminal) min_faves:%d -filter:replies' % MIN_FAVES,
    '"coding agent" min_faves:%d -filter:replies' % MIN_FAVES,
    '"pair programming" min_faves:%d -filter:replies' % MIN_FAVES,
    '("peer to peer" OR p2p OR NAT) (developer OR devtool OR CLI) min_faves:%d -filter:replies' % MIN_FAVES,
    '("just shipped" OR "just launched") (CLI OR terminal OR devtool) min_faves:%d -filter:replies' % MIN_FAVES,
    '"running agents" OR "agents in parallel" OR "multiple agents" min_faves:%d -filter:replies' % MIN_FAVES,
    '(rust OR zig) (TUI OR terminal OR ratatui) min_faves:%d -filter:replies' % MIN_FAVES,
    '"show hn" (terminal OR CLI OR agent) min_faves:%d -filter:replies' % MIN_FAVES,
    '(ssh OR "remote dev") (frustrating OR annoying OR painful) min_faves:%d -filter:replies' % MIN_FAVES,
    'ghostty OR wezterm OR alacritty OR warp min_faves:%d -filter:replies' % MIN_FAVES,
    '"my terminal" OR "terminal setup" OR "dev setup" min_faves:%d -filter:replies' % MIN_FAVES,
]


def is_niche(text):
    low = (text or "").lower()
    if any(w in low for w in NOISE):
        return False
    return any(w in low for w in NICHE)


def search(query, key):
    posts, cursor = [], ""
    for _ in range(PAGES_PER_QUERY):
        data = call(SEARCH, {"query": query, "queryType": "Top", "cursor": cursor}, key)
        posts.extend(data.get("tweets") or [])
        if not data.get("has_next_page"):
            break
        cursor = data.get("next_cursor") or ""
        if not cursor:
            break
    return posts


def main():
    key = os.environ.get("TWITTERAPI_KEY")
    if not key:
        sys.exit("Set TWITTERAPI_KEY first: export TWITTERAPI_KEY=...")

    authors = defaultdict(lambda: {
        "followers": 0, "name": "", "bio": "", "hits": 0, "niche_hits": 0,
        "queries": set(), "best": None, "best_faves": -1,
    })
    dead = []

    for q in QUERIES:
        label = q.split(" min_faves")[0]
        print(f"searching: {label[:58]:58}", end=" ", flush=True)
        try:
            posts = search(q, key)
        except RateLimited as e:
            print(f"RATE LIMITED ({e}) — rerun or raise X_MIN_INTERVAL")
            dead.append(label)
            continue
        except Exception as e:                      # noqa: BLE001 - report and continue
            print(f"FAILED ({e})")
            dead.append(label)
            continue
        print(f"{len(posts):3} posts")
        if not posts:
            dead.append(label)
        for p in posts:
            a = p.get("author") or {}
            handle = field(a, "userName", "screen_name", "username")
            if not handle:
                continue
            rec = authors[handle]
            rec["followers"] = field(
                a, "followers", "followersCount", "follower_count", default=0) or 0
            rec["name"] = field(a, "name", default="") or ""
            rec["bio"] = (field(a, "description", "bio", default="") or "")[:200]
            rec["hits"] += 1
            rec["queries"].add(label)
            text = p.get("text") or ""
            if is_niche(text):
                rec["niche_hits"] += 1
            faves = p.get("likeCount") or 0
            if faves > rec["best_faves"]:
                rec["best_faves"] = faves
                rec["best"] = {"url": p.get("url"), "text": text[:280]}

    sized = [
        (h, r) for h, r in authors.items()
        if MIN_FOLLOWERS <= r["followers"] <= MAX_FOLLOWERS
    ]
    # Rank by how many of their surfaced posts are actually in-niche. Volume in the
    # niche beats breadth across queries: an account with 39 on-topic posts talks to
    # your buyer daily, while one that grazed two broad queries just has reach.
    for h, r in sized:
        r["relevance"] = r["niche_hits"] / r["hits"] if r["hits"] else 0.0
    sized.sort(key=lambda kv: (kv[1]["niche_hits"], len(kv[1]["queries"]),
                               kv[1]["best_faves"]), reverse=True)
    strong = [(h, r) for h, r in sized if r["niche_hits"] >= 2 and r["relevance"] >= 0.5]
    weak = [(h, r) for h, r in sized if (h, r) not in strong]

    def block(fh, rows):
        for h, r in rows:
            fh.write(f"## @{h} — {r['followers']:,} followers "
                     f"({r['niche_hits']}/{r['hits']} on-topic posts, "
                     f"{len(r['queries'])} queries)\n\n")
            if r["bio"]:
                fh.write(f"- {r['bio']}\n")
            if r["best"]:
                fh.write(f"- best post ({r['best_faves']:,} likes): {r['best']['url']}\n")
                fh.write("```\n" + r["best"]["text"].strip() + "\n```\n")
            fh.write("\n")

    with open("candidates.md", "w") as fh:
        fh.write(f"# Candidate accounts\n\n{len(strong)} strong, {len(weak)} weak.\n\n")
        fh.write("Strong = at least 2 on-topic posts and over half their surfaced posts "
                 "in-niche. Still cull by hand: keep accounts whose audience is your "
                 "buyer, not accounts you find interesting.\n\n")
        fh.write(f"# Strong ({len(strong)})\n\n")
        block(fh, strong)
        fh.write(f"# Weak — skim, mostly noise ({len(weak)})\n\n")
        block(fh, weak)

    with open("candidates.txt", "w") as fh:
        fh.write("# Strong candidates. Review candidates.md, delete the misses,\n")
        fh.write("# then append what survives to accounts.txt\n")
        for h, _ in strong:
            fh.write(f"{h}\n")

    print(f"\n{len(strong)} strong, {len(weak)} weak -> candidates.md, candidates.txt")
    if dead:
        print(f"Queries that returned nothing: {len(dead)} — {', '.join(d[:30] for d in dead)}")


if __name__ == "__main__":
    main()
