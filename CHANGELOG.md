# Changelog

Newest first. Versions are the tags on [Releases](https://github.com/pelazas/p2pmux/releases).

## Compatibility

**v0.1.8, v0.1.9 and v0.1.10 share sessions.** The wire protocol has not moved since v0.1.8, so a
peer on any of the three can join the others. v0.1.7 and older cannot join any of them: that pin
moved in v0.1.8 — a machine now tells the session it has joined a fleet, and a joining machine can
present an enrolment token — and a peer on the wrong side of it is refused rather than half-joining.
From v0.1.10 that refusal says so in as many words, naming both protocol numbers and which machine
is the old one; older peers still report it as a host they could not reach.

## v0.1.10 — 2026-08-16

What your machines do while nobody is watching.

A machine that joined a session and was never opened on used to claim it was watching a pane —
whichever one its layout started on, which belongs to somebody else's laptop. It appeared on their
tab bar and lit that pane's border as watched. Presence has always been able to say "attached to
nothing"; nothing ever said it. A node says it now, when no terminal is attached and again the
moment one leaves, so detaching also takes the dot with it.

The fleet stops costing a session its responsiveness. Deciding whether to follow an invitation
walked the local session store, and that walk probes every session's socket and waits a quarter of
a second for an answer — including this node's own socket, which is answered by the loop that is
blocked doing the probing, so it never answers. On a node with one paired machine that was 41% of
the main thread, which is also the thread that echoes keystrokes. Those callers now read the
records without probing them, and this user's id is asked of the system once rather than forked per
call.

An invitation is acted on once a minute rather than on every announcement. Machines re-announce
every couple of seconds on purpose, so a machine that was asleep hears about a session started
while it was — but a session that cannot be joined was being retried thirty times a minute
forever, and each attempt spawned a node that died. Those nodes are now waited on instead of left
as zombies, and a follow that fails says so in the session log rather than only in a status line
nobody is there to read.

`docs/USAGE.md` gains a list of what to check when a machine will not join, and the per-release
protocol-pin paragraphs move here, where the compatibility table already answers that question.

## v0.1.9 — 2026-08-15

The first command, and the rows you cannot act on.

Bare `p2pmux` now always ends in a session: a session already serving a terminal is one to pass
over rather than a failure to report, so a second window on a machine that already has p2pmux open
rejoins the paired session, or creates one if this machine is on its own, instead of stopping at
`Error: already attached`. When the rejoin has to dial a machine that is asleep it says so before
spending the thirty seconds, not after. The listing is `p2pmux list`, with `ls` kept as an alias,
and `p2pmux attach` takes the name optionally.

In the inbox, an agent in *another* p2pmux session on the same machine is drawn dim and carries the
command that reaches it: the cursor walks past it, a click on it opens nothing, and it is left out
of the `inbox N` badge — the badge counts summonses somebody can answer from here. The elapsed
clock now dates whichever state a row is in rather than only a working one, so `needs you` says how
long it has been waiting and an interrupted turn stops restarting its clock.

An emoji presentation glyph no longer eats the character beside it and shifts everything after it,
Shift+Tab reaches the pane instead of being swallowed, and the wheel is aimed by the pointer rather
than by focus. A refused local connection is no longer taken as proof the node is gone, one bad
local connection cannot end a session, and a node that dies says what happened.

## v0.1.8 — 2026-08-11

The fleet, and the inbox telling the truth.

A machine you pair while a session is already open now stays in the fleet — it used to announce
that it belonged to none for as long as that p2pmux ran, so nothing ever wrote it down and its row
vanished with the session. One machine is one row, however many p2pmux it has run.

`p2pmux enroll` puts a machine you own in your fleet from a provisioning script, with a revocable
token instead of a code somebody types within ten minutes. `p2pmux work` is how a machine says what
your other machines may start on it, which until now could only be written by hand into a file most
people never found — and a refusal names the command that lifts it, on the machine to run it on.

Agents in *another* p2pmux session on the same machine are named as such rather than called
"running outside p2pmux": the row is drawn dim, the cursor and the pointer both pass over it, and
it carries the command that reaches that agent — `p2pmux attach <name>`, from a terminal, since a
p2pmux nested in a pane of another one is not a way in. The `inbox N` badge stops counting an agent
once you have been to its pane, and never counts one you cannot get to from here. `m` moves the
cursor into the fleet and the arrow keys walk it.

## v0.1.7 — 2026-08-10

An agent running outside p2pmux — one you started in another terminal, or a bot under systemd — now
reports *what it is doing*, so the inbox shows it working, blocked or done rather than listing a
process it knows nothing about: its hooks leave a record on the machine, and the scan that found
the process reads it back.

Dragging a selection past a pane's top or bottom scrolls it, and keeps scrolling for as long as you
hold it there, so what you copy is no longer limited to what fits on screen. And when a newer
release exists the inbox says so, naming the one command that fits how your copy was installed.

## v0.1.6 — 2026-08-09

The other machines.

The inbox tells a machine you own from a person who joined, and only ever offers to start work on
the first kind. You can open a terminal on one of your machines from the fleet list, subject to an
allowlist that machine's owner writes on the machine itself — commands matched in full, default
closed, and no blocklist, because a blocklist on an interactive shell is a guardrail against
accidents and not a boundary. Your machines follow you into sessions they were never paired into,
kept there by a fleet agent under launchd or systemd.

Agents running *outside* p2pmux — a bot under systemd, something in a stray tmux — appear in the
inbox too, and pressing enter opens their own chat client on their own machine. Hermes and OpenClaw
are detected; the row says which of the two things enter does, because `openclaw chat` joins the
conversation its gateway is having and `hermes chat` starts a new one.

## v0.1.5 and earlier

See the [release notes](https://github.com/pelazas/p2pmux/releases) for tags v0.1.0 through v0.1.5.
