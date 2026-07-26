use std::{
    fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use p2pmux::agent_detect::{AgentKind, SysinfoSampler, classify_pane_tree, sample_global_snapshot};

#[test]
fn sysinfo_sampler_classifies_a_live_node_pi_child() {
    if !Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let temporary_root = std::env::temp_dir().join(format!(
        "p2pmux-agent-detect-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after epoch")
            .as_nanos()
    ));
    let script = temporary_root.join("pi-coding-agent/dist/cli.js");
    fs::create_dir_all(script.parent().expect("script has a parent")).expect("create script tree");
    fs::write(&script, "setInterval(() => {}, 1_000);\n").expect("write script");
    let mut child = Command::new("node")
        .arg(&script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn live Node child");
    let child_pid = child.id();
    let mut sampler = SysinfoSampler::default();
    let mut last_child = None;

    let detected = (0..20).find_map(|_| {
        let snapshot = sample_global_snapshot(&mut sampler);
        let child_seen = snapshot.iter().find(|process| process.pid == child_pid);
        last_child = child_seen.cloned();
        let result = classify_pane_tree(std::process::id(), &snapshot);
        if child_seen.is_some() && result.is_some() {
            result
        } else {
            thread::sleep(Duration::from_millis(25));
            None
        }
    });

    let _ = child.kill();
    let _ = child.wait();
    fs::remove_dir_all(temporary_root).expect("remove script tree");
    assert_eq!(
        detected.map(|agent| agent.kind),
        Some(AgentKind::Pi),
        "live child snapshot: {last_child:?}"
    );
}

#[test]
fn sysinfo_sampler_captures_a_running_cursor_agent_argv_when_present() {
    let mut sampler = SysinfoSampler::default();
    let snapshot = sample_global_snapshot(&mut sampler);
    let Some(cursor) = snapshot.iter().find(|process| {
        process
            .cmdline
            .iter()
            .any(|argument| argument.contains("cursor-agent"))
    }) else {
        return;
    };
    let cursor_only = vec![cursor.clone()];
    assert_eq!(
        classify_pane_tree(cursor.pid, &cursor_only).map(|agent| agent.kind),
        Some(AgentKind::Cursor)
    );
}
