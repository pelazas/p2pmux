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
| 2026-08-16 | [ratatui/awesome-ratatui](https://github.com/ratatui/awesome-ratatui/pull/407) | List entry, Development Tools | PR open |
| 2026-08-16 | [iroh Show and tell](https://github.com/n0-computer/iroh/discussions/4474) | Discussion post | Posted |
| 2026-08-16 | GitHub topics | Added `claude-code`, `iroh`, `ratatui`, `ai-agents`, `peer-to-peer`, `remote-pairing`, `cli` | Live |

## What the lists told us about positioning

Worth more than the three PRs. awesome-ratatui's **Development Tools** section is full of tools
that supervise coding agents — `bosun`, `claudectl`, `crmux`, `iris`, `thurbox`, `trex`, `ilmari`,
`amtr` — and every one of them watches agents **in tmux, on one box**. That is a crowded, active
category with people browsing it, and p2pmux's inbox is the only entry in it that spans machines.

The pair-programming framing puts us next to tmate, where the demand is falling and the field is
commoditized. The agent-supervision framing puts us next to a category that is being actively
shopped, as the one thing in it that is not single-box. Lead with the inbox.

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

### 4. awesome-claude-code — the biggest surface available

[hesreallyhim/awesome-claude-code](https://github.com/hesreallyhim/awesome-claude-code), **52,000
stars**, updated daily. We qualify: the bar is 14 days old with continued commits, or 100 stars, and
p2pmux is 23 days old and committed to daily.

**It has to be you, in a browser.** The CONTRIBUTING file says submissions must go through the web
UI issue form and warns that using `gh` risks being blocked from the repo:
[submit form](https://github.com/hesreallyhim/awesome-claude-code/issues/new?template=recommend-resource.yml).
One resource per submission.

Pitch it as the agent tool, not the multiplexer — the list is explicitly for things that use Claude
Code's own features, and the hook integration is exactly that:

> p2pmux gives you one inbox listing every Claude Code session running across every machine you
> own — laptop, desktop, droplets — sorted by which one is blocking you. Press Enter on a row and
> you are typing in that session's terminal. State comes from Claude Code's own hooks, installed by
> `p2pmux setup`, so `needs you` means the agent said so rather than an inference from output
> timing; an agent with no hooks says *state unknown* instead of being guessed about. Underneath it
> is a peer-to-peer terminal multiplexer where each pane is a PTY on its owner's machine, so the
> sessions keep running on the machines that own them and their credentials never move.

Read the maintainer's note first — they are blunt that getting listed is not a growth strategy and
that they prefer projects that already have users. Submit it, but do not wait on it.

### 5. console.dev

Send [console-dev-submission.md](./console-dev-submission.md) to hello@console.dev. It is a beta
tools newsletter, so **this window closes the day 1.0 is tagged** — stable releases are explicitly
ineligible.

### 6. Terminal Trove

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

## Distribution that is not posting

Posting is the obvious half and the smaller one. These are worth more per hour.

**The invite is the growth loop, and it was leaking.** Every session invites up to seven people, and
each one is handed `p2pmux join <code>` — on a machine that has never had p2pmux, that is
`command not found` and the trail ends there. Fixed: the panel and the clipboard now both carry the
install line. This is the only channel that compounds, because it is fed by use rather than by
posting, so anything that widens it beats another list.

Still open in the same vein, in order of how much they are worth:

1. **AUR.** Arch's user repository has no notability gate at all, unlike Homebrew core (75 stars)
   and nixpkgs. People browse and search it specifically to find tools. Needs an AUR account and an
   SSH key — an hour, once, and then it is a permanent shelf we are on.
2. **asciinema.** Host the three-machine cast on asciinema.org and embed it. A real cast beats a GIF
   for a terminal tool: it is selectable text, it is a third of the bytes, and asciinema's own
   browse page is a discovery surface the GIF in the README will never be.
3. **AlternativeTo.** Nobody searches "p2pmux". They search *tmate alternative*. Listing against
   tmate, tmux and Zellij puts us in front of demand that already exists and is already qualified.
4. **`p2pmux --version` and the installer are touchpoints too.** Anywhere the binary already talks
   to a new machine is a place the repo can be named for free.

## Judgement call, not yet made

Zellij and tmate both have long-running threads from people asking for exactly what p2pmux does.
That is pre-qualified demand sitting in one place. It is also someone else's issue tracker, and
"I built the thing you are asking for" lands as help or as spam depending entirely on the thread and
on how it is written. Worth doing in at most one or two threads, in your own words, only where
somebody is actually asking. Not something to automate, and not something to do at volume.

## Hacker News

Off the table by decision, not by oversight. Do not post it there.
