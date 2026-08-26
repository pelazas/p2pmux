use std::process::Command;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[test]
fn environ_of_a_child_is_readable() {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "sleep 5"])
        .env("P2PMUX_SOCK", "/tmp/p2pmux-probe/n.sock")
        .env("P2PMUX_PANE_ID", "7")
        .spawn()
        .expect("spawn");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = Pid::from_u32(child.id());
    let mut system = System::new();
    let started = std::time::Instant::now();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
    );
    let one = started.elapsed();
    let process = system.process(pid).expect("process");
    let environ: Vec<String> = process
        .environ()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| value.starts_with("P2PMUX"))
        .collect();
    println!("targeted refresh took {one:?}; markers: {environ:?}");

    // And the cost of asking for every process at once, for comparison.
    let mut all = System::new();
    let started = std::time::Instant::now();
    all.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    let without = started.elapsed();
    let mut all2 = System::new();
    let started = std::time::Instant::now();
    all2.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    let with = started.elapsed();
    println!(
        "full snapshot: without environ {without:?}, with environ {with:?}, processes {}",
        all2.processes().len()
    );
    let _ = child.kill();
    assert!(
        environ
            .iter()
            .any(|v| v.contains("/tmp/p2pmux-probe/n.sock")),
        "markers: {environ:?}"
    );
}
