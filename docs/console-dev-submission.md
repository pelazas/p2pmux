# Console.dev submission — draft

Send to **hello@console.dev**. Edit it so it sounds like you before sending — it is short
on purpose, and the details are what matter, not the pitch.

Eligibility checked 2026-08-02: Console only covers pre-1.0 / beta tools, so v0.1.14
qualifies. Stable releases are explicitly *not* eligible, which means this window closes
the day you tag 1.0.

---

**Subject:** p2pmux — terminal multiplexer where each pane runs on its owner's machine

Hi,

I've been building p2pmux, a terminal multiplexer for people who work with coding agents
on more than one machine. It's at v0.1.14, MIT, Rust.

The difference from tmux/tmate: there's no shared box. Every pane runs a PTY on the
machine of whoever opened it, with that machine's PATH, env and subscriptions. You join a
session with a ten-character code and get a grid where some panes are on your laptop and
some are on your teammate's. Nothing is uploaded to anyone — panes are streamed
peer-to-peer, end-to-end encrypted, over an iroh relay when NAT requires it.

The case that made me build it: two people, two Claude subscriptions, one session.
Your teammate drives Claude Code in a pane hosted on your machine, on your subscription,
and never sees your key. The pane next to it is on their box with their Python env.

Install is one line on macOS or Linux, no account, no server to run:

    curl -fsSL https://p2pmux.com/install.sh | sh

- Repo: https://github.com/pelazas/p2pmux
- Site: https://p2pmux.com
- Trust model (what a session actually grants): https://github.com/pelazas/p2pmux#trust

It's early — v0.1.x, and peers have to be within one minor protocol pin of each other,
so a session across two machines on different versions can refuse to join. Happy to
answer anything.

Carlos

---

## Notes before you send

- **The trust-model link is deliberate.** It says plainly that a session is a fully trusted
  shared shell and not a sandbox. Console's criteria include security and privacy, and
  linking the honest version reads better than omitting it.
- **The status caveat stays in.** They cover beta tools; pretending it is finished helps
  nobody and gets found out on first install.
- Cut the Claude paragraph if it reads as bandwagon — the locality argument stands alone.
