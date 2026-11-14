#![cfg(unix)]

#[test]
fn malformed_first_connect_has_a_specific_error() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_morsh"))
        .args([
            "--ssh",
            "/bin/sh -c 'printf \"MOSH CONNECT broken\\nMOSH CONNECT 60001 abcdefghijklmnopqrstuv\\n\"'",
            "--remote-shell=posix",
            "127.0.0.1",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Bad MOSH CONNECT string"), "{stderr}");
    assert!(!stderr.contains("installed on the remote host"), "{stderr}");
}
