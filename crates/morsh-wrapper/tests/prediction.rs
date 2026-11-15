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
