---
name: p2pmux
description: >
  List other coding agents in a p2pmux session, start work on a paired
  machine, and wait until that pane finishes. Use when handing work to
  another agent, checking who is running, or spawning on another machine
  in the fleet.
metadata:
  owner: p2pmux
---

# p2pmux

You are in or beside a p2pmux session. The same CLI a script uses is how
you talk to other agents. Do not type into another agent's pane.

## See who is running

```
p2pmux ctl agents
p2pmux ctl machines
```

JSON on stdout. `agents` is the inbox roster: id, pane, kind, host, state,
`needs_you`. `machines` is the paired machines you may start work on.

If ctl fails, there is no live session on this machine. Start `p2pmux`
first, or say you cannot see the others.

## Start work

Put the task on the command line of a new pane. Do not `p2pmux ctl send`
into an agent that is already running.

```
p2pmux ctl spawn --new --machine NAME -- claude -p "the task"
```

`NAME` comes from `ctl machines`. Without `--new`, a live pane titled
`chat: {command}` on that host is reused; `--new` is what two jobs need.

Wait until that pane finishes:

```
p2pmux ctl events
```

JSON lines: a snapshot, then `needs_you` / `done` / `error`. Stop when
the `pane_id` you spawned is `done` or `error`.

`spawn` only starts a command on a machine you own, and only if that
command is on its allowlist. Nested `p2pmux` is refused.

If the spawn JSON has `visible_to_guests: true`, someone unpaired is in
the session and can see the pane. Do not put secrets on that command
line.

## Do not

- `p2pmux ctl send` into another agent. That is typing. It fails when
  someone else holds the pane, and it is the wrong way to hand off work.
- Start `p2pmux` inside a pane.
- Assume an agent on someone else's machine can be spawned onto. `spawn`
  names a paired machine, not a guest.
