use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .output()
        .expect("p2pmux binary should run")
}

#[test]
fn join_rejects_an_invalid_ticket_without_echoing_it() {
    let output = run(&["join", "not-a-ticket"]);

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stdout.contains("TRUST WARNING"));
    assert!(stdout.contains("fully trusted shared-shell session"));
    assert!(stderr.contains("invalid ticket format"));
    assert!(!stdout.contains("not-a-ticket"));
    assert!(!stderr.contains("not-a-ticket"));
}

#[test]
fn join_requires_a_ticket_argument() {
    let output = run(&["join"]);

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("<TICKET>"));
}

#[test]
fn help_lists_the_local_terminal_command() {
    let output = run(&["--help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("local"));
    assert!(stdout.contains("local interactive shell"));
    assert!(stdout.contains("reusable shared-session ticket"));
    assert!(stdout.contains("remote fixed-grid shared pane"));
}
