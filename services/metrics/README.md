# Metrics

A Cloudflare Worker backed by one D1 database. It stores **one anonymous row per install
per day** and nothing else, so that p2pmux can answer the one question GitHub cannot:
did anybody come back.

It is **not** on the connectivity path and never touches a session. A machine that never
sends a ping works exactly the same as one that does.

## Why this exists

Every number here is a proxy. Downloads say a tarball was fetched — by a
person, a mirror, or CI, and nothing distinguishes them. Stranger-opened issues say
somebody was annoyed enough to type, which is real but slow and heavily filtered. Neither
tells you whether the person who installed p2pmux on Monday started a session on Thursday,
and that is the number that decides whether this is a product.

## The schema is the privacy policy

```
id · day · version · os · sessions · peers · agents · activated
```

That is the whole table. There is no column for a hostname, a path, a session name, an
IP, or a byte anyone typed, so there is no setting that starts collecting one — a change
of mind here would be a schema migration and a code review, not a config flag.

- **`id`** is 32 random hex characters generated on first run and stored in
  `~/.config/p2pmux/telemetry-id`. It is deliberately **not** derived from the machine key
  in `src/machine_id.rs`: that key is announced to peers and signed over the peer id, and a
  metrics row linkable to it would be a metrics row linkable to an identity. Delete the
  file and this install becomes a new one.
- **`day`** is stamped by the server from its own clock. A laptop whose clock is a year
  fast cannot write a row into next August.
- **`activated`** is sticky, and set the first time a session on that install reached two
  members. One person in a p2pmux session is using a worse tmux; two is the product. This
  is the number the roadmap turns on.

**Your IP reaches this Worker**, because one reaches every web server. It is used to rate
limit writes and is never written to the database. "Not stored" is the honest claim;
"not seen" would be a lie any traceroute disproves.

**Nothing is sent unless the person said yes.** p2pmux asks once, on first run, and a
machine that declined — or that was never asked, because it had no terminal to ask in —
makes no request at all. So these numbers **undercount real use**, permanently and by an
unknown factor, and must never be quoted as if they were a census. What they are good for
is a *ratio* over time, where the undercount mostly cancels.

## API

| Method | Path | Behaviour |
| --- | --- | --- |
| `GET` | `/` | Plain-text description of exactly what is collected |
| `POST` | `/p` | Store one ping. `204`, `400` on a malformed body, `413` over 1 KiB, `429` when rate limited |
| `GET` | `/stats?k=` | Aggregate numbers as JSON. `404` without the key |

`POST /p` takes the eight fields above as JSON, minus `day`. Every field is validated
against a pattern before it reaches SQL: an id that is not 32 hex characters, a version
that is not `X.Y.Z`, or an OS that is not a flattened target triple is a `400`.

Counters **add** on conflict rather than replace. The client zeroes its counters only
after a send succeeds, so a second ping on the same UTC day carries what happened since
the first one and replacing it would throw that away. `activated` can only go up.

Writes are limited to 15/minute per IP. That is generous on purpose: a real install writes
one row a day, so the limit bounds a script rather than shaping traffic, and an office
behind one NAT where six people installed p2pmux is exactly the population it would be
worst to silently drop.

## Reading the numbers

```sh
curl -s "https://m.p2pmux.com/stats?k=$P2PMUX_STATS_KEY" | python3 -m json.tool
```

`retention_w3_pct` is measured on installs first seen 14–28 days ago and asks how many ran
p2pmux in the last seven. It is `null` until that cohort has anybody in it — zero percent
retention and nobody old enough to measure are different facts, and a launch week reads
the first one as a verdict.

The endpoint is behind a key because these are the numbers a decision gets made on, and
publishing them was not the reason for collecting them. There is a fair argument for
opening it: a tool that asks people to send numbers is in a stronger position when it
shows them what they added up to. Deleting `statsAllowed` and its one call site is the
whole change.

## Deploying

```sh
export CLOUDFLARE_API_TOKEN=...      # Workers Scripts:Edit, D1:Edit, and on the zone DNS:Edit + Zone:Read
export CLOUDFLARE_ACCOUNT_ID=...
npx wrangler@4 deploy
npx wrangler@4 secret put STATS_KEY
```

The database in `wrangler.toml` was created once with
`npx wrangler@4 d1 create p2pmux-metrics`, and `schema.sql` applied with
`npx wrangler@4 d1 execute p2pmux-metrics --remote --file schema.sql`.

## Verifying a deploy

```sh
B=https://m.p2pmux.com
ID=$(python3 -c "import secrets;print(secrets.token_hex(16))")
curl -s $B/ | head -3                                                     # the description
curl -s -o /dev/null -w '%{http_code}\n' -X POST -d '{"bad":1}' $B/p      # 400
curl -s -o /dev/null -w '%{http_code}\n' -X POST $B/p \
  -d "{\"id\":\"$ID\",\"version\":\"0.0.0\",\"os\":\"linux-x86_64\",\"sessions\":1}"   # 204
curl -s -o /dev/null -w '%{http_code}\n' $B/stats                         # 404 without the key
```

A test row is a real row. Delete it afterwards:

```sh
npx wrangler@4 d1 execute p2pmux-metrics --remote \
  --command "DELETE FROM ping WHERE version = '0.0.0'"
```
