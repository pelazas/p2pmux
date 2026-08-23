-- One row per install per day. Everything the metrics page asks is a query over this.
--
-- The shape is deliberate. Three separate events — installed, active, activated —
-- would be three tables to join and three chances for a client to send two of them
-- and not the third. One row a day collapses all three: a row existing means that
-- install ran p2pmux that day, the first row for an id is the install, and
-- `activated` is a flag on the row rather than an event of its own.
--
-- `day` is assigned by the server from its own clock, not sent by the client. A
-- machine whose clock is a year fast cannot write a row into next August.
--
-- Absent on purpose: IP addresses, hostnames, session names, ticket bytes, and
-- anything a person typed. The rate limiter sees a client IP the way every web
-- server does; nothing here writes one down.
CREATE TABLE IF NOT EXISTS ping (
  -- 32 lowercase hex. Random, generated on first run, and unrelated to the machine
  -- key in src/machine_id.rs — that one is announced to peers, and a metrics row
  -- that could be tied to it would be a metrics row tied to an identity.
  id           TEXT    NOT NULL,
  day          TEXT    NOT NULL,
  version      TEXT    NOT NULL,
  os           TEXT    NOT NULL,
  sessions     INTEGER NOT NULL DEFAULT 0,
  peers        INTEGER NOT NULL DEFAULT 0,
  agents       INTEGER NOT NULL DEFAULT 0,
  -- Sticky, and the number the roadmap turns on: set the first time a session on
  -- this install reached two members. One person in a p2pmux session is using a
  -- worse tmux; two is the product.
  activated    INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id, day)
);

-- The actives query is `WHERE day >= ...`, which is the only scan that grows with
-- time rather than with installs.
CREATE INDEX IF NOT EXISTS ping_by_day ON ping (day);
