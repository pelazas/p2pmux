"""Shared twitterapi.io client: global throttle + soft-error detection.

Two traps this exists to handle.

1. The free tier allows one request every 5 seconds, and exceeding it does NOT return
   HTTP 429. It returns HTTP 200 with an empty `tweets` list and an explanatory
   `message`. Naive code reads that as "no results" and silently drops whole accounts
   or queries from the study — which is how you end up analysing a swipe file built
   from a third of your list without knowing it.
2. The throttle has to be global across all calls in the process, not a sleep at one
   call site, or paginating inside a loop quietly breaks it.

Set X_MIN_INTERVAL to something lower (e.g. 0.3) after topping up past the free tier.
"""

import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "https://api.twitterapi.io"
MIN_INTERVAL = float(os.environ.get("X_MIN_INTERVAL", "5.2"))

_last_call = [0.0]


class RateLimited(Exception):
    """The API soft-failed with a rate-limit message behind a 200."""


def _throttle():
    wait = MIN_INTERVAL - (time.monotonic() - _last_call[0])
    if wait > 0:
        time.sleep(wait)
    _last_call[0] = time.monotonic()


def call(path, params, key, tries=5, retry_empty=2):
    """GET an endpoint, retrying transport errors, soft rate limits, and blank pages.

    `retry_empty` covers a third failure mode: the API intermittently answers 200 with
    an empty `tweets` list and no message at all, for a query that returns results when
    retried seconds later. Nothing in the response distinguishes it from a genuinely
    empty query, so the only defence is to ask again. An empty response bills at the
    minimum per-request charge, so a couple of retries cost effectively nothing.
    """
    url = f"{BASE}{path}?{urllib.parse.urlencode(params)}"
    req = urllib.request.Request(url, headers={"x-api-key": key})
    empties = 0
    for attempt in range(tries):
        _throttle()
        try:
            with urllib.request.urlopen(req, timeout=45) as resp:
                data = json.loads(resp.read().decode())
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503, 504) and attempt < tries - 1:
                time.sleep(5 * (attempt + 1))
                continue
            raise
        except urllib.error.URLError:
            if attempt < tries - 1:
                time.sleep(5 * (attempt + 1))
                continue
            raise

        msg = (data.get("message") or "").lower()
        soft_limit = "qps" in msg or "rate limit" in msg or "too many" in msg
        if soft_limit and not data.get("tweets"):
            if attempt < tries - 1:
                time.sleep(6 * (attempt + 1))
                continue
            raise RateLimited(data.get("message", "rate limited"))
        if data.get("status") == "error" and not data.get("tweets"):
            raise RuntimeError(data.get("message", "unknown API error"))
        if not data.get("tweets") and empties < retry_empty:
            empties += 1
            time.sleep(3 * empties)
            continue
        return data
    raise RateLimited("exhausted retries")


def field(obj, *names, default=None):
    """Read the first key present — the author object is not fully documented."""
    for n in names:
        if isinstance(obj, dict) and obj.get(n) is not None:
            return obj[n]
    return default
