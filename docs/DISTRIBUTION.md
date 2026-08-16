# Distribution

Where p2pmux has been posted, and the copy that is ready to post next. One row per submission, so
the same channel never gets hit twice by accident.

The claims rules in [LAUNCH_KIT.md](./LAUNCH_KIT.md) §2 apply to everything on this page. The one
that gets forgotten: *processes and credential files stay on the host machine* and *a controller
has a real shell as you* are both true, and they go in the same breath every time.

## Log

| Date | Channel | What | Status |
| --- | --- | --- | --- |
| 2026-08-16 | [n0-computer/awesome-iroh](https://github.com/n0-computer/awesome-iroh/pull/61) | List entry, Collaboration and Productivity | PR open |
| 2026-08-16 | [rothgar/awesome-tuis](https://github.com/rothgar/awesome-tuis/pull/833) | List entry, Productivity | PR open |
| 2026-08-16 | [iroh Show and tell](https://github.com/n0-computer/iroh/discussions/4474) | Discussion post | Posted |

## Blocked on an account

Everything below is written and checked. It needs a login that only Carlos has.

### 1. r/rust — the highest-value one

This is now the **only** route into This Week in Rust. As of Issue 664 the TWiR editors no longer
take Projects/Tooling links by pull request; they pull them from r/rust and weigh community
upvotes ([announcement](https://github.com/rust-lang/this-week-in-rust/issues/8575)). One post here
is two channels.

Check the subreddit's current flair list before posting and pick the project/showcase one. Attach
`docs/assets/workflow.mp4` — the three-machine inbox clip is the strongest asset we have and it
needs no second human to explain.

> **Title:** p2pmux: a terminal multiplexer where every pane is a PTY on its owner's own machine
>
> tmux, tmate, SSH and cloud dev environments all collapse onto one box: one machine runs
> everything, one person's toolchain is the only one in the room, and one person's keys pay for it.
> I wanted the machines to stay separate and only the surface to be shared, so I wrote p2pmux.
>
> Zellij-like tabs, panes and nested splits, except every pane is a PTY on the machine of whoever
> opened it, with that machine's shell, PATH, env and subscriptions. You join with a ten-character
> code and land in one grid where some panes are on your laptop and some are on a teammate's, on
> their network and their OS. Take control of a free pane by typing into it; active typing is
> protected, so there is no forced takeover.
>
> It is useful with nobody else in the room, which is how I actually use it. Bare `p2pmux` opens an
> inbox listing every Claude Code, Codex, Cursor, Pi and OpenCode agent running on every machine in
> the session, sorted by which one is blocking a human. One laptop and two droplets is a normal
> session. `needs you` comes from the agents' own hooks, never from guessing at output timing — an
> agent with no hooks says *state unknown* on its row rather than being guessed about.
>
> Rust throughout: ratatui for the UI, iroh for transport. Panes stream peer-to-peer and
> end-to-end encrypted, direct when hole-punching works and over a relay when NAT says no; the tab
> bar prints which you got, `direct 55ms` or `relayed 120ms`. Coordinator failover is in — if the
> laptop that started the session closes, panes elsewhere keep running and the earliest-joined
> survivor takes the role over after five minutes.
>
> Be clear about what a session is: a trusted shared shell, not a sandbox. Your processes and
> credential files never leave your machine, and whoever holds a pane of yours can run anything you
> can run. Both halves are true; the README's trust model says exactly where the line is.
>
> macOS and Linux, both architectures, MIT.
>
> ```sh
> curl -fsSL https://p2pmux.com/install.sh | sh
> ```
>
> https://github.com/pelazas/p2pmux
>
> v0.1.10, early but real — sessions run between continents today. The design question I would most
> like argued with: a keystroke has to round-trip before the character appears, so the relay path is
> felt in a way a file transfer never is. Everything about the latency budget follows from that.

### 2. r/commandline

Different crowd, lower bar, and they respond to a GIF above all else. Lead with
`docs/assets/demo.gif` and keep the text to roughly this.

> **Title:** I made a terminal multiplexer where each pane runs on a different computer
>
> Every pane is a PTY on the machine of whoever opened it — their shell, their PATH, their env,
> their installed tools. You join a session with a ten-character code and get one grid of tabs and
> splits where some panes are on your laptop and some are on someone else's box on another network.
> Type into a free pane to take control of it.
>
> It is not a shared screen and not a remote box: nobody's environment moves, and there is no server
> to run. Panes stream peer-to-peer and end-to-end encrypted, with a relay only when NAT requires
> it. Worth being blunt about the trust model — a session is a trusted shared shell, so whoever
> holds a pane on your machine can run anything you can. Your keys stay on your disk, but do not
> hand the code to a stranger.
>
> macOS and Linux, MIT, Rust: `curl -fsSL https://p2pmux.com/install.sh | sh`
>
> https://github.com/pelazas/p2pmux

### 3. iroh Discord

<https://iroh.computer/discord>, `#showcase` or whatever the current show-your-work channel is
called. The exact audience: people who already know why NAT traversal is hard. Post the short form
and link the discussion rather than repeating it.

> Built a terminal multiplexer on iroh 1.0.3 — every pane is a PTY on its owner's own machine, so a
> session is one grid spanning several computers instead of everyone SSHing into one box. Direct
> when hole-punching works, relayed when NAT says no, and the tab bar prints which you got with the
> live RTT, which turned out to be the single most reassuring bit of UI in the thing.
>
> I had budgeted weeks for writing and running the rendezvous half myself and iroh deleted the whole
> work item. Wrote it up here: https://github.com/n0-computer/iroh/discussions/4474 — repo is
> https://github.com/pelazas/p2pmux (MIT, macOS + Linux).

### 4. console.dev

Send [console-dev-submission.md](./console-dev-submission.md) to hello@console.dev. It is a beta
tools newsletter, so **this window closes the day 1.0 is tagged** — stable releases are explicitly
ineligible.

### 5. Terminal Trove

<https://terminaltrove.com/submit/> — a web form, so it cannot be automated. A durable listing on a
site people browse specifically to find terminal tools; worth the five minutes.

## Not yet eligible

Do not submit these until the gate is met. Both would be closed on sight today, and a closed PR is
worse than no PR.

| List | Gate | Where we are |
| --- | --- | --- |
| [agarrharr/awesome-cli-apps](https://github.com/agarrharr/awesome-cli-apps) | >20 stars, >3 months old, and it explicitly does not accept AI-written PRs — this one has to be written by hand | 2 stars, created 2026-07-24 |
| [rust-unofficial/awesome-rust](https://github.com/rust-unofficial/awesome-rust) | >50 stars or >2000 crates.io downloads | 2 stars |

Two more lists were checked and rejected as dead rather than ineligible:
`alebcay/awesome-shell` (last commit 2025-08, 185 open PRs) and `k4m4/terminals-are-sexy`
(last commit 2024-07, 147 open PRs). A merge there is not coming.

## Hacker News

Off the table by decision, not by oversight. Do not post it there.
