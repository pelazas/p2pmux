#!/usr/bin/env python3
"""Pull recent posts for a list of X accounts and keep each account's outperformers.

Usage:
    export TWITTERAPI_KEY=...
    python3 fetch_winners.py accounts.txt

Writes winners.json (full raw post objects) and winners.md (ranked, readable).

Why "outperformer" is measured against the account's own median rather than a global
number: a 200k-view post from an account that averages 300k is a flop, and a 20k-view
post from an account that averages 2k is the thing you want to study. Median rather than
mean because a single viral post drags the mean above every other post in the sample,
which would leave you with a swipe file of one.
"""

import json
import os
import statistics
import sys

from xapi import call

API = "/twitter/user/last_tweets"
PAGES_PER_ACCOUNT = 3      # 20 posts per page
MIN_POSTS = 10             # below this, a median is meaningless
WINNER_MULTIPLE = 2.0      # keep posts with >= 2x the account's median views


def fetch_account(handle, key):
    """Return original (non-reply) posts for one handle, newest first."""
    posts, cursor = [], ""
    for _ in range(PAGES_PER_ACCOUNT):
        data = call(API, {"userName": handle, "cursor": cursor}, key)
        batch = data.get("tweets") or []
        posts.extend(p for p in batch if not p.get("isReply"))
        if not data.get("has_next_page"):
            break
        cursor = data.get("next_cursor") or ""
        if not cursor:
            break
    return posts


def quality(post):
    """Replies + reposts + bookmarks per view.

    The algorithm weights these above likes, so this ranks posts by the engagement
    that actually moves a post outward rather than by raw popularity.
    """
    views = post.get("viewCount") or 0
    if not views:
        return 0.0
    weighted = (
        (post.get("replyCount") or 0)
        + (post.get("retweetCount") or 0)
        + (post.get("bookmarkCount") or 0)
    )
    return weighted / views


def main():
    key = os.environ.get("TWITTERAPI_KEY")
    if not key:
        sys.exit("Set TWITTERAPI_KEY first: export TWITTERAPI_KEY=...")

    path = sys.argv[1] if len(sys.argv) > 1 else "accounts.txt"
    with open(path) as fh:
        handles = [
            line.strip().lstrip("@")
            for line in fh
            if line.strip() and not line.startswith("#")
        ]

    winners, failed, thin, seen_keys = [], [], [], set()

    for i, handle in enumerate(handles, 1):
        print(f"[{i}/{len(handles)}] @{handle}", end=" ", flush=True)
        try:
            posts = fetch_account(handle, key)
        except Exception as e:                      # noqa: BLE001 - report and continue
            print(f"FAILED ({e})")
            failed.append(handle)
            continue

        viewed = [p for p in posts if (p.get("viewCount") or 0) > 0]
        if len(viewed) < MIN_POSTS:
            print(f"skipped, only {len(viewed)} posts with view counts")
            thin.append(handle)
            continue

        median = statistics.median(p["viewCount"] for p in viewed)
        cut = median * WINNER_MULTIPLE
        hits = [p for p in viewed if p["viewCount"] >= cut]
        for p in hits:
            p["_handle"] = handle
            p["_median_views"] = median
            p["_ratio"] = round(p["viewCount"] / median, 2)
            p["_quality"] = round(quality(p), 4)
            seen_keys.update(p.keys())
        winners.extend(hits)
        print(f"{len(hits)} winners of {len(viewed)} (median {int(median):,} views)")

    winners.sort(key=lambda p: p["_ratio"], reverse=True)

    with open("winners.json", "w") as fh:
        json.dump(winners, fh, indent=2, ensure_ascii=False)

    with open("winners.md", "w") as fh:
        fh.write(f"# Swipe file — {len(winners)} outperformers "
                 f"from {len(handles) - len(failed) - len(thin)} accounts\n\n")
        fh.write("Ranked by how far each post beat its own account's median views.\n\n")
        for p in winners:
            fh.write(f"## @{p['_handle']} — {p['_ratio']}x median "
                     f"({p['viewCount']:,} views)\n\n")
            fh.write(f"- {p.get('createdAt','?')} | likes {p.get('likeCount',0):,} "
                     f"| replies {p.get('replyCount',0):,} "
                     f"| reposts {p.get('retweetCount',0):,} "
                     f"| bookmarks {p.get('bookmarkCount',0):,} "
                     f"| quality {p['_quality']}\n")
            fh.write(f"- {p.get('url','')}\n\n")
            fh.write("```\n" + (p.get("text") or "").strip() + "\n```\n\n")

    print(f"\n{len(winners)} winners -> winners.json, winners.md")
    if failed:
        print(f"Failed handles (check spelling): {', '.join(failed)}")
    if thin:
        print(f"Too few posts to rank: {', '.join(thin)}")
    media_keys = sorted(k for k in seen_keys if any(
        w in k.lower() for w in ("media", "video", "photo", "entit", "extended")))
    if media_keys:
        print(f"Media-ish fields present in the raw data: {', '.join(media_keys)}")


if __name__ == "__main__":
    main()
