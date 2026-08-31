/**
 * p2pmux metrics — one anonymous row per install per day, and nothing else.
 *
 * This exists because every number p2pmux currently reports is a proxy. Release
 * asset downloads say someone fetched a tarball; stranger-opened issues say
 * someone was annoyed enough to type. Neither says whether anybody came back on
 * Tuesday, and retention is the only number that decides whether this is a
 * product or a demo.
 *
 * What it receives is bounded by what it stores, which is bounded by the table in
 * schema.sql: a random id, the version, the OS, three small counters, and a flag.
 * There is no field here for a hostname, a path, a session name, or a byte anyone
 * typed, so there is no configuration of this Worker that starts collecting one.
 *
 * Two things are worth saying plainly rather than leaving to be discovered:
 *
 *  - **A client IP reaches this Worker**, because one reaches every web server.
 *    It is used to rate limit writes and is never written to the database. The
 *    honest claim is "not stored", not "not seen", and the second one would be a
 *    lie any traceroute disproves.
 *  - **Nothing is sent unless the person said yes.** The consent lives in the
 *    client (src/telemetry.rs), asked once on first run, and a machine that
 *    declined or was never asked makes no request at all — so this Worker's
 *    numbers undercount real use, on purpose, and should be read that way.
 *
 * The day is stamped here rather than sent, so a wrong clock on one laptop cannot
 * put rows in next month, and the client has one less thing to get right.
 */

/** The client's id: exactly what `secrets.token_hex(16)` produces, and nothing else. */
const ID_PATTERN = /^[0-9a-f]{32}$/;

/** `0.1.12`, or `0.1.12-rc1` from someone running a pre-release. */
const VERSION_PATTERN = /^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]{1,16})?$/;

/** `macos-aarch64`, `linux-x86_64`. Target triples, flattened. */
const OS_PATTERN = /^[a-z0-9_]{1,16}-[a-z0-9_]{1,16}$/;

/**
 * Bound on one day's counters.
 *
 * Not a guess about heavy use — it is a cap on how much one row can distort an
 * average if a client ever ships a counting bug. A machine that honestly started
 * more than this many sessions in a day is a machine whose exact number does not
 * change any decision.
 */
const MAX_COUNT = 10_000;

/** A ping is eight small fields. Anything larger is not one. */
const MAX_BODY_BYTES = 1024;

const LANDING = `p2pmux metrics.

p2pmux can send one anonymous row a day, and only if you said yes when it asked
on first run. The row is: a random id generated on your machine, the version, the
OS, how many sessions you started, how many peers joined them, how many agent
notifications fired, and whether a session ever reached two members.

That is the whole schema. There is no field for a hostname, a directory, a
session name, or anything you typed, and terminal traffic never comes near this
service — it goes peer to peer.

Your IP reaches this server, because one reaches every web server. It rate limits
writes and is never stored.

Run \`p2pmux telemetry show\` to print the exact row this machine would send, and
\`p2pmux telemetry off\` to stop sending it.

https://github.com/pelazas/p2pmux/blob/main/services/metrics/README.md
`;

/** No body, no headers worth varying: every failure looks the same from outside. */
function status(code) {
  return new Response(null, { status: code });
}

function text(body, code = 200) {
  return new Response(body, {
    status: code,
    headers: { 'content-type': 'text/plain; charset=utf-8' },
  });
}

function json(body, code = 200) {
  return new Response(JSON.stringify(body, null, 2), {
    status: code,
    headers: { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' },
  });
}

/** A counter the table will accept: a whole number, not negative, not absurd. */
function count(value) {
  const n = Number(value ?? 0);
  if (!Number.isFinite(n) || n < 0) {
    return 0;
  }
  return Math.min(Math.floor(n), MAX_COUNT);
}

/**
 * The UTC date this request arrived on.
 *
 * UTC rather than anything local because the alternative is a boundary that moves
 * with whoever is awake, and a "day" that means two different spans on two rows is
 * not a day.
 */
function today() {
  return new Date().toISOString().slice(0, 10);
}

/**
 * Rate limit by client IP.
 *
 * Writes only, and generously: a legitimate install writes one row a day, so the
 * limit exists to bound a script rather than to shape real traffic. Fifteen a
 * minute leaves room for a shared NAT — an office where six people all installed
 * p2pmux is a plausible burst, and throttling it would silently undercount exactly
 * the population worth knowing about.
 */
async function writeAllowed(request, env) {
  if (!env.WRITE_LIMIT) {
    return true; // `wrangler dev --local` has no rate-limit binding to bind.
  }
  const key = request.headers.get('cf-connecting-ip') ?? 'unknown';
  const { success } = await env.WRITE_LIMIT.limit({ key });
  return success;
}

/**
 * Fold one ping into today's row for that install.
 *
 * Counters add rather than replace. A client flushes and zeroes its counters only
 * after a send succeeds, so a second ping on the same UTC day is carrying what
 * happened since the first one, and replacing would throw it away. `activated` can
 * only ever go up, because it describes something that happened once and stays
 * true. Version and OS take the newest value, which is what an upgrade looks like.
 */
const UPSERT = `
INSERT INTO ping (id, day, version, os, sessions, peers, agents, activated)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT (id, day) DO UPDATE SET
  version   = excluded.version,
  os        = excluded.os,
  sessions  = MIN(ping.sessions + excluded.sessions, ${MAX_COUNT}),
  peers     = MIN(ping.peers    + excluded.peers,    ${MAX_COUNT}),
  agents    = MIN(ping.agents   + excluded.agents,   ${MAX_COUNT}),
  activated = MAX(ping.activated, excluded.activated)
`;

/**
 * The four numbers GitHub cannot provide, plus the series behind
 * them.
 *
 * Retention is measured on a cohort that has had time to churn: installs first
 * seen between 14 and 28 days ago, and how many of those ran p2pmux in the last
 * seven. Measuring it on everyone would count today's signups as retained and
 * flatter every week a launch lands in.
 */
const SUMMARY = `
WITH first_seen AS (SELECT id, MIN(day) AS d0 FROM ping GROUP BY id),
     recent     AS (SELECT DISTINCT id FROM ping WHERE day >= date('now', '-6 days')),
     cohort     AS (SELECT id FROM first_seen
                    WHERE d0 <= date('now', '-14 days') AND d0 > date('now', '-28 days'))
SELECT
  (SELECT COUNT(*) FROM first_seen)                                        AS installs,
  (SELECT COUNT(*) FROM recent)                                            AS active_7d,
  (SELECT COUNT(DISTINCT id) FROM ping WHERE activated = 1)                AS activated,
  (SELECT COUNT(*) FROM first_seen WHERE d0 >= date('now', '-6 days'))     AS new_7d,
  (SELECT COUNT(*) FROM cohort)                                            AS cohort,
  (SELECT COUNT(*) FROM cohort WHERE id IN (SELECT id FROM recent))        AS cohort_retained
`;

const DAILY = `
SELECT day,
       COUNT(*)        AS active,
       SUM(sessions)   AS sessions,
       SUM(peers)      AS peers,
       SUM(agents)     AS agents
FROM ping
WHERE day >= date('now', '-29 days')
GROUP BY day
ORDER BY day DESC
`;

const VERSIONS = `
SELECT version, COUNT(DISTINCT id) AS installs
FROM ping
WHERE day >= date('now', '-6 days')
GROUP BY version
ORDER BY installs DESC
`;

/**
 * Whether this request may read the numbers.
 *
 * Behind a key rather than open, because these are the numbers a decision gets
 * made on and publishing them was not the point of collecting them. Deleting this
 * function and its one call site is all it takes to make the page public, and
 * there is a good argument for doing that: a tool that asks people to send numbers
 * is in a stronger position when it shows them what they added up to.
 */
function statsAllowed(url, env) {
  const key = env.STATS_KEY;
  if (!key) {
    return false; // No secret configured means no reading, not open to everyone.
  }
  const offered = url.searchParams.get('k') ?? '';
  // Constant time is overkill against a network attacker who can only guess at
  // request rate, but the comparison is free and the alternative invites a
  // question that would take longer to answer than to rule out.
  if (offered.length !== key.length) {
    return false;
  }
  let difference = 0;
  for (let i = 0; i < key.length; i += 1) {
    difference |= offered.charCodeAt(i) ^ key.charCodeAt(i);
  }
  return difference === 0;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === '/' || url.pathname === '') {
      return request.method === 'GET' ? text(LANDING) : status(405);
    }

    if (url.pathname === '/stats') {
      if (request.method !== 'GET') {
        return status(405);
      }
      if (!statsAllowed(url, env)) {
        return status(404); // Not 403: an absent page is a page nobody probes.
      }
      const [summary, daily, versions] = await env.DB.batch([
        env.DB.prepare(SUMMARY),
        env.DB.prepare(DAILY),
        env.DB.prepare(VERSIONS),
      ]);
      const totals = summary.results[0] ?? {};
      const rate = (part, whole) => (whole > 0 ? Math.round((part / whole) * 1000) / 10 : null);
      return json({
        installs: totals.installs ?? 0,
        new_7d: totals.new_7d ?? 0,
        active_7d: totals.active_7d ?? 0,
        activated: totals.activated ?? 0,
        activation_rate_pct: rate(totals.activated ?? 0, totals.installs ?? 0),
        // Null rather than zero while the cohort is empty. Zero percent retention
        // and nobody old enough to measure are different facts, and a launch week
        // reads the first one as a verdict.
        retention_w3_pct: rate(totals.cohort_retained ?? 0, totals.cohort ?? 0),
        retention_cohort: totals.cohort ?? 0,
        daily: daily.results,
        versions: versions.results,
      });
    }

    if (url.pathname !== '/p') {
      return status(404);
    }
    if (request.method !== 'POST') {
      return status(405);
    }
    if (!(await writeAllowed(request, env))) {
      return status(429);
    }

    const body = await request.arrayBuffer();
    if (body.byteLength === 0 || body.byteLength > MAX_BODY_BYTES) {
      return status(413);
    }

    let ping;
    try {
      ping = JSON.parse(new TextDecoder().decode(body));
    } catch {
      return status(400);
    }
    if (ping === null || typeof ping !== 'object') {
      return status(400);
    }
    // Validated rather than merely inserted: this is what keeps the table to the
    // shape schema.sql describes, so the service cannot be turned into somewhere
    // to write arbitrary strings by a client that decides to send some.
    if (!ID_PATTERN.test(ping.id ?? '')) {
      return status(400);
    }
    if (!VERSION_PATTERN.test(ping.version ?? '') || !OS_PATTERN.test(ping.os ?? '')) {
      return status(400);
    }

    await env.DB.prepare(UPSERT)
      .bind(
        ping.id,
        today(),
        ping.version,
        ping.os,
        count(ping.sessions),
        count(ping.peers),
        count(ping.agents),
        ping.activated === true ? 1 : 0,
      )
      .run();

    return status(204);
  },
};
