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
        "--delete-session",
        "--rename-session",
        "--export-session",
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
    assert!(stdout.contains("Pi Rust"));
    assert!(stdout.contains("你好"));
}

#[test]
fn file_flag_appends_file_contents_to_prompt() {
    let dir = std::env::temp_dir().join(format!("pi-rust-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let payload = dir.join("note.txt");
    std::fs::write(&payload, "alpha-beta-gamma").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pi"))
        .args([
            "--print",
            "--no-tools",
            "--file",
            payload.to_str().unwrap(),
            "请总结",
        ])
        .output()
        .expect("run pi --file");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Echo provider reflects the prompt; assert file contents made it into
    // the user message envelope.
    assert!(stdout.contains("alpha-beta-gamma"));
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
