//! Ad-hoc probe: what does the process sampler actually see for the p2pmux
//! nodes on this machine, and does the ancestor walk find them?
//!
//! Not a test. It exists to answer one question about issue #109 — whether
//! `is_node_process` can fail on a live node because `sysinfo` gave us no
//! command line for it.
//!
//! Run: cargo run --release --example probe_nodes

fn main() {
    let mut sampler = p2pmux::agent_detect::SysinfoSampler::default();
    let processes = p2pmux::agent_detect::sample_global_snapshot(&mut sampler);
    println!("{} processes sampled", processes.len());

    let mut nodes = 0;
    for process in &processes {
        let looks_like_p2pmux =
            process.name.contains("p2pmux") || process.exe_basename.contains("p2pmux");
        if !looks_like_p2pmux {
            continue;
        }
        nodes += 1;
        println!(
            "pid={} parent={:?} name={:?} exe={:?} argc={} cmdline={:?}",
            process.pid,
            process.parent_pid,
            process.name,
            process.exe_basename,
            process.cmdline.len(),
            process.cmdline,
        );
    }
    println!("{nodes} p2pmux-looking processes");

    let scan = p2pmux::agent_detect::AgentScan::new(&processes);
    for process in &processes {
        let agentish = ["claude", "opencode", "codex"]
            .iter()
            .any(|kind| process.name.contains(kind));
        if !agentish {
            continue;
        }
        println!(
            "agent pid={} name={:?} enclosing_node={:?}",
            process.pid,
            process.name,
            scan.enclosing_node(process.pid),
        );
    }
}
