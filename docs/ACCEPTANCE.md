# Acceptance criteria — the machines and agents issues (#71–#76)

What "done" means for each of the six open issues, written before any code was.
Each criterion is a thing that can be checked: a test that exists, a command
whose output is stated, or a screen whose contents are asserted. "It compiles"
is not on this list.

Verified against the real world before writing, because two of the issues turn
on facts nobody had checked:

- The Hermes agent on `mybotvm` runs as
  `/home/pelazas/.hermes/hermes-agent/venv/bin/python -m hermes_cli.main gateway run --replace`.
  Its `ps` basename is `python`, which is why detection cannot key on the
  executable name alone.
- `hermes chat` runs the agent **in the calling process**. It does not connect
  to the running gateway. `--continue` resumes a stored session; there is no
  flag that joins the conversation the gateway is having right now.
- `openclaw chat` **does** go through the gateway daemon on `localhost:18789`,
  and `--local` is the flag that opts out of it.

So the guess in #76 that both agents are `attach` is half right, and the
capability table has to record that rather than paper over it.

---

## #71 — Distinguish a machine you own from a person who joined

**Ownership rule shipped.** A member is *your machine* only when both hold:
it is named in this machine's own pairing file, **and** it declared itself a
machine when it joined. A peer that declares itself a person is never your
machine, whatever the pairing file says. Self-declaration can only narrow the
set, never widen it, and that direction is asserted by a test.

1. `MemberKind` (machine | person) rides `Join` and `MemberDescriptor`, survives
   encode/decode, and an unknown value decodes as `person` — the safe end.
   Covered by a protocol round-trip test.
2. `machine_rows` on Home and in `p2pmux machines` return rows carrying
   ownership, and the two agree row-for-row. One test drives both paths off one
   fixture and asserts the same answer.
3. Home draws owned machines and session guests differently, and the difference
   is asserted against a rendered screen, not against a struct field.
4. `p2pmux machines` prints guests under a heading that is not "your machines",
   or omits them; either way a guest is never presented as fleet.
5. **The `remember_peers` hole is closed.** Today any peer in a session, once
   this machine is paired, is written into the pairing file — a guest who joined
   with a code you handed out becomes fleet. After this issue, only peers that
   declared `machine` are recorded. Regression test: a person-kind peer joins a
   paired session and the pairing file is unchanged.
6. No stranger can promote themselves. Test: a peer declaring `machine` whose
   name is *not* in the pairing file renders as a guest.

## #72 — Open a new terminal on a remote machine

1. `CreatePane` and `CreateTab` carry an optional target peer. Absent means
   "here", and every existing caller keeps working — asserted by the existing
   layout and session tests continuing to pass unchanged.
2. A pane whose target is another member commits with `host_peer_id` set to that
   member, not to the requester. Layout-level test.
3. The target peer spawns the pty and reports `PaneReady`; the requester spawns
   nothing. Session-level test with two peers, asserting which side created the
   pty and that the pane streams to the requester.
4. A target that is not a member, or not a machine you own (#71), is rejected
   with a distinct reason rather than silently falling back to a local pane.
5. Home has a machine picker: a machine row can be selected, and the key that
   opens a terminal on it is documented in the footer. Asserted on a rendered
   screen.
6. Failure states are sentences, not error codes. Machine asleep, machine
   refused, machine has no p2pmux — each has its own message, each asserted.
7. **End to end on real hardware**: from this laptop, open a terminal on
   `mybotvm`, run `hostname` in it, and see `mybotvm` come back.

## #74 — Per-machine safeguards, set by the machine's owner

Shipping options 1 and 3 from the issue. No blocklist ships, in any form.

1. The launch allowlist lives in a file on the machine it governs, next to the
   pairing record, and is never read from or written by another peer. Test: a
   peer's request cannot change it.
2. Default closed. A machine that never answered refuses remote panes and says
   so; `accepts_work` staying `false` is not enough on its own, an allowlist
   entry is also required.
3. An allowlisted entry names what may be launched. A remote pane starts that
   command, not a login shell, unless the owner explicitly allowlisted a shell.
4. Confirm-on-the-owning-machine: when configured to ask, the request is held,
   the owner is prompted on their own machine, and a request nobody answers
   expires rather than being granted. Test for the timeout path.
5. `p2pmux machines` shows what each machine allows, and the answer for machines
   other than this one is `—` rather than a guess.
6. Nothing in the UI describes any of this as protection against a hostile user
   of an allowed command. The word used is what it is: consent.

## #76 — Detect OpenClaw and Hermes

1. `AgentKind` gains `OpenClaw` and `Hermes`. `from_process`, `wire_value`,
   `from_wire` and `display_label` all move together, and `from_wire` still
   refuses values it does not know.
2. Hermes matches the real daemon shape: a `python` process whose cmdline
   carries `hermes_cli`, or an executable named `hermes`. Test uses the exact
   argv observed on `mybotvm`.
3. OpenClaw matches a `node` process whose cmdline carries an `openclaw`
   program path, or an executable named `openclaw`. Legacy `clawd` included.
4. No false positives from a human typing about these agents: `vim openclaw.md`,
   `hermes claw migrate` and `grep hermes_cli` are each asserted **not** to be
   detected.
5. Detection does not assume the agent is a child of a p2pmux pane. A daemon
   under systemd on another machine appears in the inbox.
6. The capability table records, for each: the chat command, and which of #75's
   categories it is. Recorded values are the ones confirmed above —
   OpenClaw `attach` via `openclaw chat`, Hermes `new session` via
   `hermes chat`. If an implementation finds Hermes can attach after all, the
   table changes and the finding is written into the issue.
7. **End to end on real hardware**: the Hermes gateway running on `mybotvm`
   appears in this laptop's inbox, named Hermes.

## #75 — Open a chat with an agent already running on another machine

1. A capability table maps agent kind → chat command → category
   (`attach` | `new session` | `none`), in one place, with every `AgentKind`
   covered. A compile-time exhaustive match, so adding a kind cannot forget it.
2. Enter on an inbox row for an agent **started inside p2pmux** still zooms to
   its pane. No regression; the existing test stays green.
3. Enter on an inbox row for an agent running **outside** p2pmux opens a remote
   terminal on that agent's machine (#72) and runs its chat command.
4. Enter twice leaves one chat pane. The second press focuses the first pane.
   Asserted.
5. The UI says which category it did, before it does it. An agent whose category
   is `new session` never presents itself as having joined the running
   conversation — this is the one bad outcome the issue names, and there is a
   test whose name says so.
6. Category `none` rows say why Enter does nothing, rather than doing nothing.
7. **End to end on real hardware**: Enter on the Hermes row from this laptop
   opens a terminal on `mybotvm` running `hermes chat`, labelled as a new
   conversation.

## #73 — Your machines follow you into every session

The biggest of the six. Done means a daemon exists, is installable, and can be
invited into a session it was not paired into.

1. `p2pmux daemon` runs the fleet agent in the foreground: it holds the fleet
   record, joins sessions it is invited to, and reconnects when the network
   returns.
2. `p2pmux daemon install` / `uninstall` writes and removes a launchd plist on
   macOS and a systemd user unit on Linux. Both are generated from one place, so
   the two platforms cannot drift.
3. Installed means: starts at boot, restarts on crash. Asserted by inspecting
   the generated unit (`KeepAlive`, `RunAtLoad`; `Restart=always`,
   `WantedBy=default.target`), and on `mybotvm` by killing the process and
   watching systemd bring it back.
4. Pairing offers to install it, at the moment the user decided this box is
   fleet. Declining is remembered and not re-asked every run.
5. **The fleet is a property of you, not of one session.** A session created
   fresh on this laptop can invite the paired droplet, and the droplet joins
   without anyone typing a code on it. This is the criterion the issue is
   actually about; the daemon is how it is met.
6. Only machines you own may invite (#71). A guest cannot summon your droplet
   into a session. Test.
7. **End to end on real hardware**: `p2pmux create` on this laptop, and
   `mybotvm` appears in the member list on its own.

---

## Global gates, all six

- `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features
  -D warnings`, `cargo test --all-features` and `cargo check --all-targets`
  green on macOS **and** Linux, which is what CI runs.
- Wire changes stay backward compatible in the one direction that matters: an
  older peer meeting a newer one degrades to the old behaviour rather than
  refusing to join.
- No feature is reported complete on the strength of a unit test alone where a
  real-hardware check is listed above.

---

# What was demonstrated

Every criterion above is met. The parts that could only be checked with a
second machine were checked with one: `scripts/e2e/scenario_r_fleet.py` pairs
this Mac with a droplet running a real Hermes gateway, over the real network,
with the droplet's p2pmux in a sandbox HOME so nothing touches a pairing record
or session anybody uses.

    == ownership (#71)          the droplet is fleet, not a guest
    == fleet follows you (#73)  it joined a session it was never paired into,
                                with no code typed there
    == detection (#76)          the Hermes gateway reached this Mac's inbox
    == remote terminal (#72)    a pane hosted by the droplet; `hostname` in it
                                prints mybotvm, not this Mac
    == chat (#75)               the Hermes row says enter starts a new
                                conversation, and never that it joins one

Separately, on the droplet's real systemd: the unit was installed, the daemon
killed at PID 443008, and systemd brought it back at 443085 with
`NRestarts=1` — #73's restart-on-crash criterion. The unit was then removed.

`scripts/e2e/scenario_af_unattended_presence.py` is the second two-machine
check, for the claim presence makes: a member is drawn as watching a pane only
while somebody is at it. The droplet joins with its terminal open and appears on
the Mac's tab bar; it then closes that terminal, leaving its node and its panes
running, and stops appearing. Pointed at a build from before the fix — with
`P2PMUX_AF_REMOTE_BINARY` — the second half fails and prints the tab bar with
the stale dot still on it, which is how the check is known to be able to fail.

## What the second machine found that the test suite could not

Six bugs, every one of them invisible to a green `cargo test`. They are the
argument for writing the hardware checks into the criteria rather than
treating them as a nice-to-have.

1. **Invitations were coalesced away.** They went out on the latest-wins state
   channel, where the next layout snapshot silently replaced them — and
   snapshots arrive constantly.
2. **The machine that offers the pairing code never entered the other
   machine's fleet.** It cannot declare itself a machine, because its node
   started before the pairing file had a ticket, and the joining side was
   filtering on that declaration. Pairing looked complete from one side and
   left the other's fleet empty.
3. **A node's peer id is not a machine's identity.** Ownership pinned to it
   held only inside the session pairing happened in, so the machine that
   followed you into a new one arrived as a stranger. This is #71's own open
   question, answered by hardware.
4. **A roster carrying an agent in no pane was dropped whole**, by a rule that
   was right and was the only rule: entries must name panes the sender hosts.
5. **Opening a terminal on another machine looked like nothing happened.** The
   tab was created, on the right machine, with the right shell — and the person
   who asked stayed where they were.
6. **The agent column named Hermes and OpenClaw "agent"**, because it kept its
   own list of kinds. #76 warned that four places move together. There were
   five.
