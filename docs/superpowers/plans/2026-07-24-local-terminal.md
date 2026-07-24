# Spike 1 — Local Terminal Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox syntax for tracking.

**Goal:** Add p2pmux local: one interactive local shell in a fixed-size PTY, parsed with vt100 and rendered with ratatui so vim, top, and a Claude-like terminal UI are suitable for manual dogfooding.

**Architecture:** cli adds a local-only entry point while preserving create/join and their trust warning. pty_host owns one portable-pty master, child, writer, and reader thread; tui drains PTY bytes into vt100::Parser, translates local keyboard/paste events to PTY bytes, and renders the parsed cell grid. The PTY and parser grid are captured once at startup and never resized: extra window area is blank and a smaller window crops the upper-left viewport.

**Tech Stack:** Rust 2024, clap 4.5, portable-pty 0.9, vt100 0.16, ratatui 0.30 with crossterm 0.29, Cargo, rustfmt, Clippy, GitHub Actions on macos-latest.

---

## Scope guard

- Implement only Spike 1 milestones 4 (pty_host local PTY spawn/read/write) and 5 (tui vt100 parse/render).
- macOS only. Use /bin/zsh when SHELL is unset.
- Do not add Iroh, tokio, prost, tickets, networking, protocol messages, session state, tabs, panes, splits, resize messages, or Spike 2+ abstractions.
- create and join <ticket> stay non-networking stubs and retain the existing fully-trusted shared-shell warning. local is the only command that starts a process and does not need that warning.
- Keep exactly one PTY, parsed screen, and viewport. Do not introduce a pane registry, layout type, controller state, or generic terminal transport.
- At local startup, turn crossterm::terminal::size() into PtySize { rows, cols, pixel_width: 0, pixel_height: 0 } and use those dimensions for both portable-pty and vt100::Parser. On later Event::Resize, do not call MasterPty::resize or Screen::set_size.
- Resize policy: the fixed grid remains upper-left aligned; larger windows leave extra cells blank and smaller windows crop it. Dynamic resize is intentionally outside Spike 1 and the MVP wire protocol.
- CI proves PTY I/O, parser, and renderer behavior without launching an interactive TUI. vim/top/Claude-like correctness is manual macOS acceptance.

## Planned file structure

| Path | Responsibility |
| --- | --- |
| docs/superpowers/plans/2026-07-24-local-terminal.md | This plan, committed first on the feature branch. |
| Cargo.toml and Cargo.lock | Adds and locks only Spike 1 dependencies. |
| src/cli.rs and src/main.rs | Adds local dispatch and propagates terminal errors without changing create/join output. |
| src/pty_host.rs | One PTY process, input writer, output reader thread, and deterministic shutdown. |
| src/tui.rs | Terminal cleanup, PTY/vt100 loop, input encoding, fixed viewport renderer, private unit tests. |
| tests/cli.rs | Preserves create/join tests and adds local-help coverage. |
| tests/module_surface.rs | Checks resource-owning PtyHost without constructing it. |
| tests/pty_host.rs | PTY input/output test with no full TUI. |
| README.md | Dogfood command, Ctrl-Q exit, and immutable-grid behavior. |
| .github/workflows/ci.yml | No edit expected: existing cargo test --all-features discovers new non-interactive tests on macOS. |

## Chunk 1: Branch, command surface, and PTY

### Task 1: Create the feature branch and commit the plan

**Files:**

- Create: docs/superpowers/plans/2026-07-24-local-terminal.md

- [ ] **Step 1: Confirm the scaffold baseline.**

Run:

~~~bash
git switch main
git status --short
git log --oneline -3
~~~

Expected: the only status entry is `?? docs/superpowers/plans/2026-07-24-local-terminal.md`; the log includes merge PR #1. Stop if any other tracked or untracked change appears. The following plan-only commit intentionally makes status clean.

- [ ] **Step 2: Create the dedicated feature branch.**

Run:

~~~bash
git switch -c spike1/local-terminal
~~~

Expected: spike1/local-terminal is checked out. Do not reuse scaffold/rust-package or PR #1.

- [ ] **Step 3: Commit this plan before source changes.**

Run:

~~~bash
git add docs/superpowers/plans/2026-07-24-local-terminal.md
git commit -m "docs: plan local terminal spike"
~~~

Expected: a plan-only commit.

### Task 2: Add local command contract and dependencies

**Files:**

- Modify: Cargo.toml
- Modify: Cargo.lock
- Modify: src/cli.rs
- Modify: src/main.rs
- Modify: src/tui.rs
- Modify: tests/cli.rs

- [ ] **Step 1: Write the failing local-help regression test.**

Append to tests/cli.rs:

~~~rust
#[test]
fn help_lists_the_local_terminal_command() {
    let output = run(&["--help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("local"));
    assert!(stdout.contains("local interactive shell"));
}
~~~

Keep every existing create/join test unchanged, including non-echoing ticket behavior.

- [ ] **Step 2: Confirm the command is absent.**

Run:

~~~bash
cargo test --test cli help_lists_the_local_terminal_command
~~~

Expected: FAIL because top-level clap help has no local subcommand.

- [ ] **Step 3: Add only Spike 1 runtime dependencies.**

Replace Cargo.toml dependencies with:

~~~toml
[dependencies]
clap = { version = "4.5", features = ["derive"] }
crossterm = "0.29"
portable-pty = "0.9"
ratatui = { version = "0.30", default-features = false, features = ["crossterm"] }
vt100 = "0.16"
~~~

Do not add tokio, iroh, prost, logging/error frameworks, or test-only process libraries. Cargo commands below update Cargo.lock.

- [ ] **Step 4: Extend clap dispatch while preserving stubs.**

Add the Local variant and arm in src/cli.rs:

~~~rust
enum Command {
    /// Start one local interactive shell (Spike 1).
    Local,
    // Keep Create and Join as the scaffold defines them.
}

match cli.command {
    Command::Local => crate::tui::run_local(),
    Command::Create => /* existing warning + stub output */,
    Command::Join { ticket } => /* existing warning + non-echoing stub output */,
}
~~~

Widen parse_and_run, its dispatcher, and main to Result<(), Box<dyn std::error::Error>> only if required to propagate PTY/TUI errors. Preserve normal nonzero error reporting. Do not print the shared-session warning for local.

Task 4 provides real run_local. At this point add only a documented compiling placeholder with that signature in src/tui.rs; replace it later, not with a second entry point.

- [ ] **Step 5: Verify CLI behavior non-interactively.**

Run:

~~~bash
cargo fmt --all
cargo test --test cli
cargo run -- --help
cargo run -- create
cargo run -- join example-secret-ticket
~~~

Expected: all succeed; help lists local; create/join still print warning plus stub notice; join output omits example-secret-ticket.

### Task 3: Implement and test one local PTY

**Files:**

- Modify: src/pty_host.rs
- Modify: tests/module_surface.rs
- Create: tests/pty_host.rs

- [ ] **Step 1: Write the failing PTY round-trip integration test.**

Create tests/pty_host.rs:

~~~rust
use std::{
    thread,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize};
use p2pmux::pty_host::PtyHost;

fn read_until(host: &mut PtyHost, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut output = String::new();
    while Instant::now() < deadline {
        while let Some(bytes) = host.try_read_output().expect("PTY reader should stay healthy") {
            output.push_str(&String::from_utf8_lossy(&bytes));
        }
        if output.contains(expected) {
            return output;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("did not receive {expected:?}; received {output:?}");
}

#[test]
fn pty_host_reads_output_and_writes_input() {
    let mut command = CommandBuilder::new("/bin/sh");
    command.args([
        "-c",
        "printf ready; IFS= read -r line; printf ':reply:%s' \"$line\"",
    ]);
    let mut host = PtyHost::spawn(command, PtySize {
        rows: 24, cols: 80, pixel_width: 0, pixel_height: 0,
    })
    .expect("PTY should spawn");

    assert!(read_until(&mut host, "ready").contains("ready"));
    host.write_input(b"hello from test\n").expect("PTY should accept input");
    assert!(read_until(&mut host, ":reply:hello from test").contains(":reply:hello from test"));
    host.shutdown().expect("PTY should shut down cleanly");
}
~~~

The production API takes CommandBuilder, so the default shell helper needs no test-only process abstraction.

- [ ] **Step 2: Verify the API is absent.**

Run:

~~~bash
cargo test --test pty_host
~~~

Expected: compile FAIL because PtyHost::spawn, try_read_output, write_input, and shutdown do not exist.

- [ ] **Step 3: Replace the marker with the resource-owning host.**

Implement this public surface in src/pty_host.rs:

~~~rust
pub struct PtyHost {
    writer: Option<Box<dyn std::io::Write + Send>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    output_rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    reader_join: Option<std::thread::JoinHandle<()>>,
}

impl PtyHost {
    pub fn spawn(
        command: portable_pty::CommandBuilder,
        size: portable_pty::PtySize,
    ) -> Result<Self, Box<dyn std::error::Error>> { /* implement below */ }

    pub fn spawn_default_shell(
        size: portable_pty::PtySize,
    ) -> Result<Self, Box<dyn std::error::Error>> { /* implement below */ }

    pub fn try_read_output(
        &mut self,
    ) -> Result<Option<Vec<u8>>, Box<dyn std::error::Error>> { /* implement below */ }

    pub fn write_input(
        &mut self,
        bytes: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> { /* implement below */ }

    pub fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> { /* implement below */ }
}

impl Drop for PtyHost {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
~~~

spawn must call native_pty_system().openpty(size), take exactly one master writer, clone exactly one master reader, spawn the supplied command from the slave, and move the reader into one thread. That thread sends non-empty chunks, then its terminal I/O result, through output_rx. Retain master, child, writer, receiver, and join handle. Import PtySystem, MasterPty, and SlavePty explicitly.

spawn_default_shell uses SHELL as OsString, falling back to /bin/zsh; launch it with -l and TERM=xterm-256color. try_read_output uses try_recv: a chunk becomes Some, empty becomes None, reader I/O error is surfaced, and disconnect is clean EOF. write_input uses write_all then flush. shutdown is idempotent: kill/reap child, drop writer/master to wake reader, and join reader exactly once using Option::take.

An unbounded standard channel is acceptable for the single local viewer because raw terminal bytes must never be discarded. Do not resize the master or add an async runtime.

In tests/module_surface.rs replace:

~~~rust
let _ = PtyHost;
~~~

with:

~~~rust
let _: Option<PtyHost> = None;
~~~

- [ ] **Step 4: Verify PTY behavior and existing contracts.**

Run:

~~~bash
cargo fmt --all
cargo test --test pty_host
cargo test --test cli
cargo test --test module_surface
cargo clippy --all-targets --all-features -- -D warnings
~~~

Expected: every command exits 0; the test proves child-to-reader and writer-to-child bytes without a terminal.

- [ ] **Step 5: Commit the PTY milestone.**

Run:

~~~bash
git add Cargo.toml Cargo.lock src/cli.rs src/main.rs src/pty_host.rs src/tui.rs tests/cli.rs tests/module_surface.rs tests/pty_host.rs
git commit -m "feat: add local pty host"
~~~

Expected: one PTY spawn/read/write commit plus local CLI surface, without a ratatui loop.

## Chunk 2: vt100 renderer and fixed viewport

### Task 4: Test and implement vt100-to-ratatui rendering

**Files:**

- Modify: src/tui.rs

- [ ] **Step 1: Add failing private renderer/input tests in src/tui.rs.**

Add a cfg(test) module using ratatui::backend::TestBackend and Terminal. It must prove:

1. b"\x1b[31;44;1mX" becomes X with indexed foreground/background colors and Modifier::BOLD.
2. A 2x3 parser renders only those six cells into a larger test backend: it never stretches or resizes the parser.
3. ESC[?1h makes Up encode as ESC O A; normal mode makes it ESC [ A.
4. Bracketed-paste mode wraps text with ESC[200~ and ESC[201~; otherwise paste is raw UTF-8.
5. A table-driven key-encoder test covers printable Unicode, Enter, Tab, Backspace, Esc, Ctrl-letter, Alt-printable, Home/End/Delete/Insert/PageUp/PageDown, F1–F12, normal/application/modified arrows, Ctrl-Q interception, and at least one unsupported event returning None.

Use this core renderer test:

~~~rust
let mut parser = vt100::Parser::new(1, 3, 0);
parser.process(b"\x1b[31;44;1mX");
let mut terminal = Terminal::new(TestBackend::new(3, 1)).unwrap();
terminal
    .draw(|frame| frame.render_widget(VtScreen::new(parser.screen()), frame.area()))
    .unwrap();
let buffer = terminal.backend().buffer();
assert_eq!(buffer[(0, 0)].symbol(), "X");
assert_eq!(buffer[(0, 0)].fg, ratatui::style::Color::Indexed(1));
assert!(buffer[(0, 0)].modifier.contains(ratatui::style::Modifier::BOLD));
~~~

- [ ] **Step 2: Confirm the tests fail.**

Run:

~~~bash
cargo test tui::tests
~~~

Expected: FAIL because VtScreen and input helpers do not exist.

- [ ] **Step 3: Directly render vt100 cells.**

Implement:

~~~rust
struct VtScreen<'a> {
    screen: &'a vt100::Screen,
}

impl<'a> VtScreen<'a> {
    fn new(screen: &'a vt100::Screen) -> Self {
        Self { screen }
    }
}

impl ratatui::widgets::Widget for VtScreen<'_> {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let (rows, cols) = self.screen.size();
        for row in 0..rows.min(area.height) {
            for col in 0..cols.min(area.width) {
                let Some(source) = self.screen.cell(row, col) else { continue };
                if source.is_wide_continuation() {
                    continue;
                }
                let target = buf.get_mut(area.x + col, area.y + row);
                target.set_symbol(source.contents());
                target.set_style(vt_style(source));
            }
        }
    }
}
~~~

vt_style exhaustively maps vt100::Color::Default, Idx, and Rgb to ratatui Color::Reset, Indexed, and Rgb; map bold, dim, italic, underline, and inverse to ratatui modifiers, including Modifier::REVERSED. Never use Screen::contents(), because it loses style, wide cells, and alternate-screen state needed by vim/top.

After render, call frame.set_cursor_position only when !screen.hide_cursor() and the cursor is inside the visible fixed viewport. Add no border, status bar, tab bar, or resize behavior.

- [ ] **Step 4: Implement the terminal lifecycle and loop.**

Replace the placeholder with pub fn run_local() -> Result<(), Box<dyn std::error::Error>> that:

1. Before raw mode, gets crossterm::terminal::size(), creates one fixed PtySize, PtyHost::spawn_default_shell(size), and vt100::Parser::new(rows, cols, 0).
2. Enables raw mode, crossterm alternate screen, and crossterm EnableBracketedPaste, then creates Terminal<CrosstermBackend<Stdout>>. Use an RAII guard so every return path, including partial setup failure, shows the cursor, disables bracketed paste, leaves alternate screen, and disables raw mode.
3. Drains at most 64 currently available try_read_output chunks (or no more than a 4 ms budget) into parser.process(&bytes), marking the frame dirty, then returns to event polling. Never discard a chunk; bounded draining prevents a chatty PTY from starving input and Ctrl-Q.
4. Polls crossterm for about 16 ms. Handle KeyEventKind::Press and Repeat; Ctrl-Q exits p2pmux and all supported other keys call write_input. Handle Event::Paste with parser bracketed-paste state. Deliberately ignore Event::Resize.
5. Draws once initially and only when dirty with VtScreen over frame.area(). The grid stays upper-left aligned; overflow is blank/cropped as locked above.
6. Exits on Ctrl-Q, PTY EOF, or error, allowing guard/host cleanup to restore terminal and terminate/reap child.

Keep key and paste encoders pure/testable. Support printable Unicode, Enter (\r), Tab, Backspace (0x7f), Esc, arrows, Home, End, Delete, Insert, PageUp, PageDown, F1 through F12, Ctrl-letter bytes, and Alt-prefixed printable keys. Respect screen.application_cursor() for unmodified cursor keys and standard modified CSI sequences for Shift/Alt/Ctrl arrows. Return None for unrepresentable events; do not create input/protocol abstractions or enable mouse input.

- [ ] **Step 5: Run the complete CI command set locally.**

Run:

~~~bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo check --all-targets --all-features
~~~

Expected: all exit 0. No workflow edit is needed: ci.yml already runs this suite non-interactively on macos-latest.

- [ ] **Step 6: Commit renderer and loop.**

Run:

~~~bash
git add src/tui.rs
git commit -m "feat: render local terminal with vt100"
~~~

Expected: one ratatui/vt100 rendering-loop commit with unit tests.

### Task 5: Document and manually validate interactive behavior

**Files:**

- Modify: README.md

- [ ] **Step 1: Document dogfooding and fixed-grid behavior.**

Add a Local Spike 1 section to README.md:

~~~markdown
Run cargo run -- local to start one local shell. Press Ctrl-Q to leave p2pmux.

The PTY grid is fixed from the terminal size at startup. Resizing the outer terminal never resizes the child shell or vt100 parser: larger windows leave extra cells blank and smaller windows crop the upper-left fixed viewport. Dynamic resize is intentionally outside Spike 1 and the MVP wire protocol.
~~~

Also say create/join remain scaffolding-only; do not imply local is networked or multi-pane.

- [ ] **Step 2: Perform the manual macOS acceptance check.**

Run:

~~~bash
cargo run -- local
~~~

Inside its shell:

1. Run vim, edit a short line, move with arrows, enter insert mode, then :q!; verify alternate screen, colors, cursor, and redraws.
2. Run top, observe several refreshes, use arrows if supported, then q; verify no smearing or bad scrolling.
3. Start an installed Claude-like terminal UI, type/submit a short prompt, then exit; verify input, cursor, colors, and alternate screen.
4. Paste a multi-line string into a prompt that accepts paste; verify it arrives once, with no visible bracket markers, and cancel it if necessary.
5. Resize outer terminal larger and smaller; verify PTY dimensions stay fixed, extra area is blank, and the top-left grid crops.
6. Press Ctrl-Q; verify original terminal has a visible cursor and usable input.

Expected: all checks pass. On failure, record app, terminal emulator, input, and rendering symptom; fix only PTY/input/renderer behavior, rerun automated checks, and repeat manual acceptance. Do not add resize or multiplayer behavior.

- [ ] **Step 3: Commit documentation.**

Run:

~~~bash
git add README.md
git commit -m "docs: document local terminal spike"
~~~

Expected: a documentation-only commit.

## Chunk 3: Final verification and new PR

### Task 6: Verify scope and open a new PR to main

**Files:**

- Modify: none

- [ ] **Step 1: Run final verification and inspect scope.**

Run:

~~~bash
git status --short
git log --oneline main..HEAD
git diff --check main...HEAD
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo check --all-targets --all-features
~~~

Expected: status empty; commits include plan, PTY, renderer, and docs; diff check empty; all Cargo commands pass. Confirm no Iroh/protocol/session/tabs/panes/resize code exists.

- [ ] **Step 2: Push and create the new Spike 1 PR.**

Run:

~~~bash
git push -u origin spike1/local-terminal
gh pr create --base main --head spike1/local-terminal --title "Spike 1 local terminal" --body "## Summary
- add one local PTY host with tested read/write behavior
- parse PTY output with vt100 and render cells in ratatui
- add p2pmux local and document fixed startup grid/no-resize policy

## Validation
- cargo fmt --all -- --check
- cargo clippy --all-targets --all-features -- -D warnings
- cargo test --all-features
- cargo check --all-targets --all-features
- manual macOS check: vim, top, and a Claude-like TUI

## Scope
Local Spike 1 only. No Iroh, networking, protocol messages, sessions, tabs, panes, or resize support."
~~~

Expected: a new PR URL with base main and head spike1/local-terminal; do not reuse PR #1.

- [ ] **Step 3: Verify PR target and macOS CI.**

Run:

~~~bash
gh pr view --json url,baseRefName,headRefName,statusCheckRollup
gh pr checks --watch
~~~

Expected: base main, head spike1/local-terminal, and passing macOS quality workflow. Handoff the new PR URL and manual visual-check result.
