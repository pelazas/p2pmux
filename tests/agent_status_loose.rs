//! An agent p2pmux did not start, reporting through its hooks.
//!
//! The unit tests either side of this one check a record round-tripping through
//! a file and a scan folding a process tree into rows. Neither checks the join:
//! that a hook with no `P2PMUX_PANE_ID` and no socket in its environment finds
//! the agent it belongs to, writes under that agent's pid, and that the scan
//! then reads it back onto that agent's row. That join is issue #78 — an inbox
//! that said `state unknown — no hooks` about agents whose hooks were installed
//! and firing.
//!
//! So this drives the real binary from a real process tree.

use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU32, Ordering},
    thread,
    time::{Duration, Instant},
};

use p2pmux::{
    agent_detect::{AgentKind, AgentScan, AgentState, SysinfoSampler, sample_global_snapshot},
    agent_status,
};

const BINARY: &str = env!("CARGO_BIN_EXE_p2pmux");

/// A fake agent that reports the way a real registration does and then waits.
///
/// It writes its own pid first because the process this test needs to name is
/// not the one it spawns: the agent is orphaned to `init` on purpose, so that
/// it is nobody's descendant and the scan sees it the way it sees an agent
/// started in another terminal. `$$` after `exec -a` is that process.
const AGENT: &str = r#"
echo $$ > "$1"
printf '{"message":"reading the tests"}' | "$2" notify claude --status running
sleep 60
"#;

struct Fake {
    pid: u32,
    root: PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .arg("-9")
            .arg(self.pid.to_string())
            .status();
        if let Some(directory) = agent_status::default_dir() {
            let _ = fs::remove_file(directory.join(format!("{}.json", self.pid)));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Start a process the scan will classify as `claude`, outside every pane and
/// descended from nothing this test is inside.
///
/// `exec -a` rather than a copy of a shell named `claude`: detection matches the
/// process's own name, a shebang script is reported as its interpreter, and
/// macOS kills a renamed copy of a signed platform binary. The detached
/// subshell is what makes it an orphan — a `claude` started underneath the
/// `claude` running this test would be folded into it as a worker, which is
/// exactly what that rule is for.
fn start_fake_agent() -> Option<Fake> {
    // One directory per agent, not one per test binary: these tests run in
    // parallel, and a shared scratch root means one test's teardown deletes the
    // pid file the other is still waiting on.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let root = std::env::temp_dir().join(format!(
        "p2pmux-loose-agent-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).expect("scratch directory");
    let script = root.join("agent.sh");
    fs::write(&script, AGENT).expect("write agent script");
    let pid_file = root.join("pid");

    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "(exec -a claude /bin/sh {} {} {} &) </dev/null >/dev/null 2>&1",
            script.display(),
            pid_file.display(),
            BINARY,
        ))
        .status()
        .expect("spawn the detached agent");
    assert!(status.success(), "the launcher itself must not fail");

    let pid = wait_for(|| {
        fs::read_to_string(&pid_file)
            .ok()
            .and_then(|raw| raw.trim().parse::<u32>().ok())
    })?;
    Some(Fake { pid, root })
}

fn wait_for<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(value) = attempt() {
            return Some(value);
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

#[test]
fn a_hook_outside_every_pane_reaches_the_row_its_agent_has_in_the_inbox() {
    let Some(directory) = agent_status::default_dir() else {
        return;
    };
    let Some(fake) = start_fake_agent() else {
        panic!("the fake agent never reported its pid");
    };

    // One: the producer found its agent by walking up from itself, and filed
    // under that agent's own process rather than giving up for want of a pane.
    let record = wait_for(|| agent_status::read(directory, fake.pid, None))
        .expect("the hook wrote a record for its agent");
    assert_eq!(record.kind, "claude");
    assert_eq!(record.state, "working");
    assert_eq!(record.message, "reading the tests");
    assert_ne!(
        record.working_since_unix_ms, 0,
        "a working agent starts the clock the inbox counts from"
    );

    // Two: the scan finds that agent, and reads the record back onto its row.
    let mut sampler = SysinfoSampler::default();
    let agent = wait_for(|| {
        let snapshot = sample_global_snapshot(&mut sampler);
        let mut loose = AgentScan::new(&snapshot).loose_agents(&[]);
        agent_status::attach(directory, &mut loose);
        loose.into_iter().find(|agent| agent.pid == fake.pid)
    })
    .expect("the scan lists an agent that is in no pane");

    assert_eq!(agent.kind, AgentKind::Claude);
    assert_eq!(
        agent.state,
        AgentState::Working,
        "the row says what the hook said, not `unknown`"
    );
    assert_eq!(agent.message, "reading the tests");
    assert_eq!(agent.working_since_unix_ms, record.working_since_unix_ms);
}

#[test]
fn a_record_is_swept_once_its_agent_is_gone() {
    let Some(directory) = agent_status::default_dir() else {
        return;
    };
    let Some(fake) = start_fake_agent() else {
        panic!("the fake agent never reported its pid");
    };
    let pid = fake.pid;
    wait_for(|| agent_status::read(directory, pid, None)).expect("a record to sweep");

    drop(fake);

    // The scan is the only clock a loose agent's status has: no process, no
    // row, no record. Without this a `needs you` from an agent that exited an
    // hour ago would still be asking.
    //
    // The predicate keeps every record but this test's own, and the grace is
    // zero: this directory belongs to the machine, not to the test, and a
    // sweep that judged the whole of it would delete a live session's statuses
    // — or the other test's, which is how this one first went red.
    let swept = wait_for(|| {
        let mut sampler = SysinfoSampler::default();
        let snapshot = sample_global_snapshot(&mut sampler);
        let scan = AgentScan::new(&snapshot);
        if scan.knows_pid(pid) {
            return None;
        }
        agent_status::sweep(directory, Duration::ZERO, |candidate| candidate != pid);
        agent_status::read(directory, pid, None)
            .is_none()
            .then_some(())
    });

    assert!(
        swept.is_some(),
        "the record outlived the process that wrote it"
    );
}
