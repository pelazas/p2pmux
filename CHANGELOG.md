# Changelog

Newest first. Versions are the tags on [Releases](https://github.com/pelazas/p2pmux/releases).

## Compatibility

**v0.1.8 through v0.1.14 share sessions.** The wire protocol has not moved since v0.1.8, so a peer
on any of the seven can join the others. v0.1.7 and older cannot join any of them: that pin moved in
v0.1.8 — a machine now tells the session it has joined a fleet, and a joining machine can present an
enrolment token — and a peer on the wrong side of it is refused rather than half-joining. From
v0.1.10 that refusal says so in as many words, naming both protocol numbers and which machine is the
old one; older peers still report it as a host they could not reach.

## Unreleased

Eight fixes, and no protocol change that costs anything: one field is added to the agent roster,
absent from older peers and degrading to what they already sent.

**Closing a p2pmux window now ends its local session.** A session stays available only after
`Ctrl+Q` then `d` (Enter still chooses detach). Closing the terminal, losing SSH, or killing the
client no longer leaves its node and panes running on this machine. Fleet-agent nodes and sleep
behavior are unchanged.

**A Linux e2e run can see the session it just wrote.** Its scratch home now covers the state path
Linux uses as well as the macOS one, so its cleanup stays inside the run rather than following a
developer's XDG setting toward a live session.

**Pane mode still says how to redraw at 100 columns.** SESSION and LOCK make room first, while
REDRAW and Esc BACK stay together; narrower terminals lose whole hints instead of a clipped word.

**A detached member comes back to its existing session.** Joining its ticket reattaches to that
member instead of minting a second one and turning the badge into two members.

**Doctor identifies the binary you ran.** It no longer calls it the latest release when PATH would
still run another copy, or when GitHub could not be asked what the latest tag is.

**An agent in another session on your machine reads as one again.** The second session's inbox
called a pane-hosted agent `running outside p2pmux` and offered to start a second copy of it. The
two halves of that row come from different places: whether an agent sits in a p2pmux pane is read
from the process table, which is machine-wide, while what that session is *called* comes from the
session store, which lives under `HOME`. Two sessions started under two `HOME`s — an ad-hoc script
with its own sandbox, a session started under `sudo`, another user of the same machine — see each
other's processes and not each other's records, so the node was found, no name was found, and the
label fell back to the one thing that was certainly untrue. The fact now travels separately from
the name: without a name the row says `another p2pmux session · no record of it under this HOME`,
and it is neither counted on the badge nor offered to Enter.

**A pane that stopped repainting has a way out.** `Ctrl+P` then `Shift+R` redraws the whole screen.
Frames carry only the cells that changed, which is what keeps a busy pane cheap and which fails
whenever something else has written to the terminal — a cluster the terminal measures differently,
a stray escape from a program in a pane, another multiplexer outside p2pmux. Until now the only cure
was resizing the window, which people found by accident. Two narrower repairs ship with it: a keycap
or a ZWJ emoji no longer reaches the terminal in a form it draws wider than the pane's grid reserved
for it, and `p2pmux local` and the two legacy remote views repaint after a resize instead of
diffing forever against a screen that moved.

**Dimming the panes you are not in is now off by default, and narrower when it is on.** How faint
reduced intensity renders is the terminal's decision rather than p2pmux's, and every mature
implementation of it ships off for that reason. Turned on, it now means the panes nobody is
reading: the pane under the pointer, one scrolled back through its own history, one a peer is
driving, and one whose agent is working or waiting on you all stay at full strength.

## v0.1.14 — 2026-08-23

Four additions and three fixes, and no protocol change: a fleet gets an address that outlives the
sessions it meets in, adding a machine becomes one command, p2pmux asks once whether it may send
one anonymous line a day, and `doctor` names which of the copies on your PATH actually runs.

**A fleet now has an address of its own, and stops being able to strand.** Until now a fleet *was*
a session: `p2pmux pair` wrote one ticket into `pairing.toml` and every machine dialled it forever.
That works exactly as long as the session it names is alive. When it ends, nothing updates the
record — and the machine that is away cannot be told, because a member only learns about new
sessions from announcements that travel inside a session it has already joined. A machine that
cannot join hears nothing, and a machine that hears nothing cannot join. It did not recover slowly;
it never recovered. Two healthy machines chased a session that had ended for four days, every
attempt reporting a host it could not reach, and the only exit was a human re-running `p2pmux pair`.

A fleet now holds 32 bytes minted once, which name one record in the same blind store the short
join codes use. That record says which session the fleet is meeting in right now: whichever machine
is hosting writes it, and a machine that cannot reach what it says publishes its own session over
the top. So the address corrects itself at the moment it turns out to be wrong, and a machine that
was switched off for a month rejoins on waking. The fleet agent gained the state this was all for —
"nobody in this fleet is hosting" is now something it can know, and wait for, rather than
something indistinguishable from chasing a corpse. It never invents a session to fill the gap.

Enrolment tokens carry the fleet rather than a session, so one pasted into a machine image months
ago no longer enrols a machine into nothing while reporting success. Failing to reach the fleet is
no longer failing to enrol, either: the record is written first and the agent keeps looking.

**Adding a machine is one command.** `p2pmux pair --token` prints the reusable invitation that
`p2pmux enroll` used to, `--revoke` withdraws it, and `p2pmux pair <code-or-token>` accepts either
form, told apart by shape. `p2pmux enroll` still works and is no longer in `--help`: it lives in
cloud-init files written months before anything runs them. Pairing also mints a code of its own
instead of reusing the session's join code — those were two credentials that happened to be one
string, and handing a collaborator the second used to hand them the first.

**Saying yes to letting your machines start work here now allows something to be started.** There
were two gates, opened at two different times by two different commands, and pairing opened one; a
machine paired with `--accept-work` refused every terminal while reporting that it accepts work.
Yes now means a login shell, which the prompt says in those words, and `p2pmux work allow
<command>` still narrows it afterwards. It never widens an allowlist somebody had narrowed.

A fleet paired before this release keeps working and cannot be given an address quietly — the
hand-off needs a channel only fleet members can read. `p2pmux machines` and the inbox now say so,
and name the one command that fixes it.

p2pmux now asks, once on first run, whether it may send one anonymous line a day: a random id, the
version, the OS, how many sessions you started, whether anybody joined one. Enter means yes,
nothing is sent before you answer, and `p2pmux telemetry show` prints the exact line at any time.
`DO_NOT_TRACK` and `CI` are honoured without being asked, and a machine with no terminal to ask in
is never asked and never sends.

Most tools collect this quietly and offer a settings key to turn it off, which gets better numbers.
That is the wrong trade for a tool whose whole claim is that your keys and processes stay on your
machine, so the cost is paid on purpose: these numbers undercount real use by an unknown amount,
permanently, and no p2pmux figure quoted anywhere should be read as a census. What it buys is the
one question downloads and issue counts cannot answer — whether the person who installed p2pmux on
Monday started a session on Thursday. The schema is eight columns in `services/metrics/schema.sql`,
there is no field for a hostname or a path or anything typed, and the id is deliberately unrelated
to the machine key that peers already see.

A machine that holds p2pmux from two channels runs one of them, and until now nothing said which.
The shell takes whichever copy comes first on PATH, which is not the one installed most recently —
so a Homebrew copy two releases behind kept winning over a fresh curl install, and every fix that
had shipped in between looked unshipped. `p2pmux doctor` now lists every `p2pmux` on PATH in the
order the shell tries them, with each one's version and a mark on the one that runs, and says so
when the winner is behind a copy installed elsewhere — naming the command that replaces *that*
copy, which is not always the command that installed the newer one. The installer makes the same
check after it copies the binary: if something else on your PATH will win, it names it, and the
install still succeeds.

Dimming the pane you are not typing into now reaches the terminal. The frame always asked for it,
and on a terminal whose environment carries `NO_COLOR` the request was cancelled a byte before the
text it applied to: crossterm answers that variable by writing a cell's colours as an SGR with no
parameters, which is a full reset, and ratatui writes colours after attributes. Bold, reverse and
underline died there too, so a selection did not look selected and a pane replaying a colourful
program lost the emphasis that program chose. p2pmux now keeps its own attributes through
`NO_COLOR`; a multiplexer draws mostly other programs' output, and each of those programs read the
same variable and already answered it.

## v0.1.13 — 2026-08-22

Six fixes and two additions, and no protocol change: what a pane draws and what you can copy out
of it, the two ways a machine rejoins a session it already belongs to, and moving focus across a
split.

Full-screen applications and `clear` erase the real terminal behind ratatui, while its cached
back-buffer can still believe old glyphs are there. The next frame now clears that outer buffer
when such output arrives, so a return from an alternate screen does not leave a ghost behind.

An attached client only keeps the scrollback pages it has recently drawn, so copying a drag that
crossed an evicted page quietly turned the missing rows into blanks. Copying now asks the local
node that owns the complete buffer; history on another machine is named as unavailable instead of
being made up.

A bare `p2pmux` on a machine whose paired session was asleep waited in silence long enough to
look wedged, then kept trying after opening a local fallback. Its rejoin now has the short window
an interactive command needs, and the fallback is remembered as the ticket's local answer. That
last part was written after the node had already started and lost a race against it; the ticket
now travels in the node's own bootstrap, so there is no window to lose it in.

A machine that was switched off when you opened a session did not join it, and did not join it
when it came back either: its own session record outlives the node that wrote it, and the fleet
agent read that record as proof it was still in its home session. It now checks that the node is
running, so a machine that has been away rejoins and catches up on whatever started while it was
gone.

`p2pmux notify idle` reached the inbox as `state unknown — no hooks`, which is the line that tells
you to run `p2pmux setup`. An agent that reports idle now reads as idle; the process scan still
decides whether the row exists at all.

Panes you are not typing into are drawn dimmed, so a glance across a split finds the focused one.
`dim_unfocused_panes = false` under `[ui]` turns it off.

`Ctrl+` arrows move focus between panes, alongside the `Option+` `<shift>` + arrows that already
did; `Ctrl+Alt+` arrows are forwarded to the shell so a readline word-jump still gets through.
Focus also stops leaving sideways: it compared pane centres, so a pane sitting diagonally counted
as being above, and an arrow that left that way did not come back.

## v0.1.12 — 2026-08-17

Six fixes and no protocol change: four in what a pane and its footer tell you, two in the agent
that keeps a fleet running.

Narrowing a pane no longer destroys the text it hides.

Dragging a split, unzooming, or attaching a second viewer with a smaller window used to cut every
visible line at the new width and throw the rest away, so widening the pane again came back to
blanks rather than to the text. Nothing could undo it: the processes that printed those lines had
already exited, and a pane's text lives only in the terminal state p2pmux keeps for it. A 300
character line put through a 118 → 38 → 118 column round trip came back as its first 38 characters.

The visible grid is now read out as logical lines before the resize and laid out again at the new
width, the way tmux, zellij and alacritty each handle it. Text that no longer fits scrolls into
scrollback instead of being dropped. An application in the alternate screen still repaints its own
frame, and a resize that changes only the row count does no work at all.

An agent in one p2pmux session now says what it needs in every other session on the same machine.

Hooks reported to one place or the other: the pane's node socket when there was one, a
machine-local record when there was not. So an agent in a pane told exactly one node what it was
doing, and the inbox of any other session on the same box — which finds it by scanning processes,
sees that it is inside a p2pmux, and names the session it is in — had nothing to say about it. A
`claude` blocked on a permission prompt read `state unknown — no hooks` to the one person who could
answer it. Hooks now write both, so the row says `needs you` wherever you are sitting. It still does
not put a number on the `inbox N` badge: going to an agent is what answers its summons, and there is
no going to that one from here.

The keybinding bar is a whole tier or nothing, never half a chord.

A footer notice is placed first and the hints are fitted into what it leaves, but the hint bar
answered a width it could not honour and was then clipped where the room ran out. A long enough
notice on a narrow enough terminal ended the bar `Ctrl+ <p` — a chord that does not exist, on the
one line that exists to say what the chords are.

Scrolling back in a pane that has no scrollback yet says nothing, instead of blaming the network.

A wheel notch on a pane nothing had scrolled off answered with one sentence naming three causes —
a remote pane, a full-screen program, an expired history — none of which described the one-second-old
shell in front of you. Each cause now says only its own sentence, and a pane with no history says
nothing at all: there is no error there, only a wheel with nowhere to go.

The fleet agent's crash-loop ceiling is now enforced rather than ignored.

`StartLimitIntervalSec` and `StartLimitBurst` were written under `[Service]`, where systemd has not
read them since v229 — it logged `Unknown key name … ignoring` and used its own defaults instead, so
the rate limit the v0.1.11 leak fix leaned on had never once applied. This matters most on systemd
older than v254, where the escalating restart backoff is itself ignored and this is the only thing
between a failing agent and twenty starts every five minutes.

An upgrade no longer leaves the fleet agent unable to start anything.

A node is launched by re-running the p2pmux binary, so an agent whose binary was replaced
underneath it — which is what every upgrade does — keeps executing an image that is no longer on
disk, and every launch fails with `No such file or directory` for as long as the process lives.
`Restart=` never fires, because nothing crashes. This is what set the v0.1.11 incident off in the
first place: the binary was replaced at 12:45:52 and the surviving agent made 1014 doomed attempts
over the next four hours. It turned up again while releasing v0.1.11, when `brew upgrade` removed
the directory the running agent was executing from. The agent now notices and stands down, and the
service manager starts it again from whatever is at that path now.

## v0.1.11 — 2026-08-17

The fleet agent stops behaving like something you would want to uninstall.

Two machines carrying a ticket for a session neither of them was hosting chased it for four days.
Every attempt was a whole operating-system process, one every fifteen seconds, and every one that
failed to start in time was left running: the launcher dropped its handle to the node, which in Rust
neither stops the process nor reaps it. Nine of those accumulated on a 4GB droplet, holding 3.3GB
between them, and when memory ran out the kernel's own killer went looking across the whole machine
for something big — so a trading bot, an API server and a message gateway were killed for p2pmux's
leak. The same loop left 1014 files in one runtime directory and 1568 in another, and wrote the same
sentence into the journal a thousand times without once suggesting anything was wrong.

A launch that produces no session now takes the node and its files with it, whatever went wrong.
The delay between attempts doubles up to five minutes and is jittered, because every machine in a
fleet loses the same coordinator at the same instant; it is a ceiling rather than a surrender, since
a home session comes back when the laptop hosting it is opened, and five minutes is also the longest
a machine should be missing from its own fleet. Failures are reported once per distinct reason and
per change of pace, and say what the pace is.

Both service files now have limits the operating system enforces, so a bug above them cannot reach
the rest of the machine — and neither restarts the agent through a clean exit any more, which means
`systemctl --user stop` and `launchctl unload` do what they say. macOS has no cgroups to enforce a
memory ceiling, so a node the agent started carries its own and stops itself before the kernel would
have to; a session you started by hand is told how large it has grown rather than stopped, because
the work in it is worth more than the memory. A node the agent started also watches the agent, and
stops when it goes: that is the only mechanism that survives the agent being killed outright, which
is exactly how the orphans were made.

**Still open, and worth knowing about.** What set all of this off is that `pairing.toml` holds one
ticket and nothing ever updates it. Start a different session on the coordinator and the record still
names the old one, so every other machine follows a ticket for a session that no longer exists — and
it cannot recover on its own, because a machine only hears about new sessions from inside a session it
has already joined. If a machine keeps reporting that it cannot reach the session host while a session
is plainly running, that is this: run `p2pmux pair` again to point the fleet at the live one.

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
