use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_p2pmux"))
        .args(args)
        .output()
        .expect("p2pmux binary should run")
}

#[test]
fn create_prints_the_trust_warning_and_stub_notice() {
    let output = run(&["create"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("TRUST WARNING"));
    assert!(stdout.contains("fully trusted shared-shell session"));
    assert!(stdout.contains("not implemented"));
}

#[test]
fn join_accepts_a_ticket_prints_the_warning_and_does_not_echo_the_ticket() {
    let output = run(&["join", "example-secret-ticket"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("TRUST WARNING"));
    assert!(stdout.contains("fully trusted shared-shell session"));
    assert!(stdout.contains("not implemented"));
    assert!(!stdout.contains("example-secret-ticket"));
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
}
