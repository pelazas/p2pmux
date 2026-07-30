/**
 * p2pmux hosted rendezvous — a blind store mapping an opaque index to an opaque blob.
 *
 * This service exists so an invite is ten characters instead of two hundred. It is not on
 * the connectivity path: iroh already provides relays and discovery, and a pasted ticket
 * dials without help from here. Terminal bytes never touch this Worker.
 *
 * It cannot read what it stores. The client derives the index from the join code and seals
 * the ticket under a separate key derived from the same code, so this code sees a hex handle
 * and a sealed blob and holds nothing that would let it join a session. That is structural
 * rather than a policy: there is no configuration of this Worker that reveals a ticket.
 *
 * Consequences worth stating plainly:
 *
 *  - Anyone who can guess an index can read and delete the blob at it. That is the same
 *    authority as knowing the code, which is the credential, so it grants nothing extra.
 *  - Single-use is deliberately not offered. KV is eventually consistent, so a
 *    delete-after-read does not propagate across edges instantly and a code could be read
 *    twice in a short window. Promising one-time codes would need Durable Objects; claiming
 *    it on top of KV would be a lie.
 */

const INDEX_PATTERN = /^[0-9a-f]{32}$/;

/** Matches the client's own cap, so an oversized body is refused before it reaches KV. */
const MAX_RECORD_BYTES = 8 * 1024;

/** KV's own floor is 60s; anything shorter is rejected by the platform, not by us. */
const MIN_TTL_SECONDS = 60;
const DEFAULT_TTL_SECONDS = 6 * 60 * 60;
const MAX_TTL_SECONDS = 24 * 60 * 60;

const LANDING = `p2pmux hosted rendezvous.

This service maps an opaque index to an opaque blob so that joining a p2pmux session
takes a short code rather than a 200-character ticket.

It cannot read what it stores. The join code never reaches this server: the client
derives the storage index from it, and seals the ticket under a separate key derived
from the same code. What arrives here is a hex handle and a sealed blob.

Terminal output, keystrokes, and session traffic never pass through this service at
all — those go peer to peer, or over an iroh relay when NAT requires it.

Records expire on their own, and a session deletes its record when it exits.

https://github.com/pelazas/p2pmux
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

function ttlFrom(url) {
  const requested = Number(url.searchParams.get('ttl'));
  if (!Number.isFinite(requested) || requested <= 0) {
    return DEFAULT_TTL_SECONDS;
  }
  return Math.min(Math.max(Math.floor(requested), MIN_TTL_SECONDS), MAX_TTL_SECONDS);
}

/**
 * Rate limit by client IP.
 *
 * Only writes are limited. Reads are the cheap side of the KV free tier by two orders of
 * magnitude, and limiting them would throttle the joiners a working session depends on
 * while doing nothing about the abuse that matters, which is filling the store.
 *
 * A guessing campaign is bounded by the code's own entropy rather than by this: a wrong
 * guess returns 404 with nothing to grind on offline.
 */
async function writeAllowed(request, env) {
  if (!env.WRITE_LIMIT) {
    return true; // `wrangler dev --local` has no rate-limit binding to bind.
  }
  const key = request.headers.get('cf-connecting-ip') ?? 'unknown';
  const { success } = await env.WRITE_LIMIT.limit({ key });
  return success;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (url.pathname === '/' || url.pathname === '') {
      return request.method === 'GET' ? text(LANDING) : status(405);
    }

    const match = url.pathname.match(/^\/r\/([^/]+)$/);
    if (!match) {
      return status(404);
    }
    const index = match[1];
    // Validated rather than merely used as a key: this bounds what can be written into the
    // namespace to exactly the shape the client derives, so the store cannot be used as
    // free general-purpose storage.
    if (!INDEX_PATTERN.test(index)) {
      return status(404);
    }

    switch (request.method) {
      case 'GET': {
        const record = await env.RECORDS.get(index, { type: 'arrayBuffer' });
        if (record === null) {
          return status(404);
        }
        return new Response(record, {
          status: 200,
          headers: {
            'content-type': 'application/octet-stream',
            // A join record is per-session and short-lived; a cached copy would outlive
            // the delete that a clean exit performs.
            'cache-control': 'no-store',
          },
        });
      }

      case 'PUT': {
        if (!(await writeAllowed(request, env))) {
          return status(429);
        }
        const body = await request.arrayBuffer();
        if (body.byteLength === 0 || body.byteLength > MAX_RECORD_BYTES) {
          return status(413);
        }
        await env.RECORDS.put(index, body, { expirationTtl: ttlFrom(url) });
        return status(204);
      }

      case 'DELETE': {
        // Unauthenticated on purpose: deriving the index already required the code, and the
        // code is the credential. Requiring a second proof would protect nothing.
        await env.RECORDS.delete(index);
        return status(204);
      }

      default:
        return status(405);
    }
  },
};
