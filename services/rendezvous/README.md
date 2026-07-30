# Hosted rendezvous

A Cloudflare Worker backed by one KV namespace. It maps an opaque index to an opaque blob so
that a p2pmux invite is a ten-character code rather than a two-hundred-character ticket.

It is **not** on the connectivity path. iroh already provides relays and node discovery, and a
pasted ticket dials without help from here. This buys UX, not capability.

## What it can and cannot see

The client derives two domain-separated values from the join code: the storage index, which is
all this service receives, and the sealing key, which never leaves the client. So the store
holds a hex handle and a sealed blob, and there is no configuration of this Worker that reveals
a ticket. See `src/hosted_rendezvous.rs` for the derivations.

Anyone who can guess an index can read and delete the blob at it. That is the same authority as
knowing the code, which is the credential, so it grants nothing extra.

Single-use codes are deliberately **not** offered. KV is eventually consistent, so a
delete-after-read does not propagate across edges instantly and a code can be read twice in a
short window. Strict single-use would need Durable Objects; claiming it on top of KV would be a
lie. Do not promise one-time codes in copy.

## API

| Method | Path | Behaviour |
| --- | --- | --- |
| `GET` | `/` | Plain-text description of the service |
| `PUT` | `/r/{index}?ttl=` | Store a sealed record. `204`, or `413` if empty or over 8 KiB, or `429` when rate limited |
| `GET` | `/r/{index}` | Return the sealed record, or `404` |
| `DELETE` | `/r/{index}` | Remove the record. Always `204` |

`{index}` must be exactly 32 lowercase hex characters — the shape the client derives. Anything
else is `404`, which keeps the namespace from being used as general-purpose storage.

TTL defaults to 6 hours and is clamped to `[60s, 24h]`. A live session refreshes its record
well inside that window and deletes it on a clean exit, so the TTL only bounds how long a
*crashed* node's record lingers.

Writes are rate limited to 30/minute per IP. Reads are not: they are the cheap side of the KV
free tier by two orders of magnitude, and limiting them would throttle the joiners a working
session depends on while doing nothing about the abuse that matters, which is filling the store.

## Deploying

```sh
export CLOUDFLARE_API_TOKEN=...      # Workers Scripts:Edit, Workers KV Storage:Edit
export CLOUDFLARE_ACCOUNT_ID=...
npx wrangler@4 deploy
```

The KV namespace id in `wrangler.toml` was created once with
`npx wrangler@4 kv namespace create RECORDS`.

## Verifying a deploy

```sh
B=https://rv.p2pmux.com
IDX=$(python3 -c "import secrets;print(secrets.token_hex(16))")
curl -s -o /dev/null -w '%{http_code}\n' $B/r/$IDX                                  # 404
printf 'blob' | curl -s -o /dev/null -w '%{http_code}\n' -X PUT --data-binary @- $B/r/$IDX  # 204
curl -s $B/r/$IDX                                                                   # blob
curl -s -o /dev/null -w '%{http_code}\n' -X DELETE $B/r/$IDX                        # 204
```

The rate limiter is per-colo and per-minute, so a *sequential* loop will not trip it. Use a
burst if you want to see it: `seq 1 50 | xargs -P 25 ...` returns a mix of 204 and 429.
