use std::process::Command;

#[test]
fn help_contains_stable_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .arg("--help")
        .output()
        .expect("run pi --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--provider",
        "--model",
        "--tools",
        "--list-providers",
        "--list-models",
        "--list-tools",
        "--list-sessions",
        "--session",
        "--resume",
        "--continue",
        "--print",
        "--no-tools",
    ] {
        assert!(stdout.contains(flag), "missing flag {flag}");
    }
}

#[test]
fn print_mode_returns_local_response() {
    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .args(["--print", "你好"])
        .output()
        .expect("run pi print mode");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pi Rust MVP"));
    assert!(stdout.contains("你好"));
}

#[test]
fn doctor_reports_environment_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .arg("doctor")
        .output()
        .expect("run pi doctor");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Pi Rust doctor"));
    assert!(stdout.contains("command\tcurl"));
    assert!(stdout.contains("provider_env\tmoonshot"));
}
