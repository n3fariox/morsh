use std::process::Command;

#[test]
fn invalid_prediction_modes_fail_before_ssh_with_stock_diagnostics() {
    for (flags, inherited, expected) in [
        (vec!["--predict=bad"], None, "morsh: Unknown mode \"bad\"."),
        (vec!["--predict="], None, "morsh: Unknown mode \"\"."),
        (
            vec![],
            Some("bad"),
            "morsh: Unknown mode \"bad\" (MOSH_PREDICTION_DISPLAY in environment).",
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_morsh"));
        command
            .args(flags)
            .args(["--ssh=morsh-nonexistent-ssh-for-prediction-test", "host"])
            .env_remove("MOSH_PREDICTION_DISPLAY");
        if let Some(mode) = inherited {
            command.env("MOSH_PREDICTION_DISPLAY", mode);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.starts_with(expected), "{stderr}");
        assert!(!stderr.contains("Failed to get MOSH CONNECT"), "{stderr}");
        assert!(!stderr.contains("failed to start SSH"), "{stderr}");
    }
}

#[test]
fn help_precedes_prediction_environment_validation() {
    let output = Command::new(env!("CARGO_BIN_EXE_morsh"))
        .arg("--help")
        .env("MOSH_PREDICTION_DISPLAY", "bad")
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[cfg(unix)]
#[test]
fn prediction_display_is_not_inherited_by_ssh_subprocesses() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("morsh-ssh-env-{}-{stamp}", std::process::id()));
    // Record config-query, shell-probe and server-launch environments. A bad
    // CONNECT ends the wrapper before a real client or network session starts.
    let script = r#"printf "%s\n" "${MOSH_PREDICTION_DISPLAY-unset}" >> "$MORSH_TEST_SSH_ENV_LOG"; if [ "$0" = "-G" ]; then printf 'proxycommand true\n'; elif [ "$1" = "uname -s" ]; then printf 'Linux\n'; else printf 'MOSH CONNECT broken\n'; fi"#;
    let ssh = format!("/bin/sh -c {}", shell_words::quote(script));
    let output = Command::new(env!("CARGO_BIN_EXE_morsh"))
        .args(["--ssh", &ssh, "host"])
        .env("MOSH_PREDICTION_DISPLAY", "always")
        .env("MORSH_TEST_SSH_ENV_LOG", &path)
        .output()
        .unwrap();
    let log = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Bad MOSH CONNECT string"));
    assert_eq!(log, "unset\nunset\nunset\n");
}
