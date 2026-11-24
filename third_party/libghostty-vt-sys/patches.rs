use std::path::Path;
use std::process::Command;

/// Apply sorted backports to an override or our disposable cached checkout.
pub fn apply_patches(source: &Path, directory: &Path, managed_checkout: bool) {
    let mut patches: Vec<_> = std::fs::read_dir(directory)
        .unwrap_or_else(|e| panic!("read {}: {e}", directory.display()))
        .map(|entry| entry.expect("patch directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "patch"))
        .collect();
    patches.sort();
    // A deterministic fingerprint of both names and contents detects edits,
    // additions, removals and ordering changes without another build dependency.
    let mut hash = 0xcbf29ce484222325_u64;
    for patch in &patches {
        let name = patch.file_name().expect("patch filename").to_string_lossy();
        let contents =
            std::fs::read(patch).unwrap_or_else(|e| panic!("read {}: {e}", patch.display()));
        for bytes in [name.as_bytes(), contents.as_slice()] {
            for byte in (bytes.len() as u64).to_le_bytes().iter().chain(bytes) {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            }
        }
    }
    let fingerprint = format!("{hash:016x}");
    let stamp = source.join(".ghostty-patches");
    if managed_checkout && std::fs::read_to_string(&stamp).ok().as_deref() != Some(&fingerprint) {
        // Only OUT_DIR checkouts are disposable. Never reset an explicit
        // GHOSTTY_SOURCE_DIR, which may contain the developer's own changes.
        // Missing stamps also migrate checkouts patched by older build scripts.
        if stamp.exists() {
            std::fs::remove_file(&stamp).expect("remove stale Ghostty patch stamp");
        }
        let mut restore = Command::new("git");
        restore.current_dir(source).args(["checkout", "--", "."]);
        run(restore, "restore cached Ghostty checkout before patching");
    }
    for patch in patches {
        apply_patch(source, &patch);
    }
    // Write only after all patches succeed. A failed application is restored
    // from the original checkout on the next attempt.
    if managed_checkout {
        std::fs::write(stamp, fingerprint).expect("write Ghostty patch stamp");
    }
}

fn apply_patch(source: &Path, patch: &Path) {
    // Ignore whitespace only in context lines, so a Windows checkout using
    // CRLF also accepts the LF patch. Arguments remain separate for paths
    // containing spaces. No Git repository or network access is required.
    let command = || {
        let mut command = Command::new("git");
        command
            .current_dir(source)
            .args(["apply", "--ignore-whitespace"]);
        command
    };
    let forward = command()
        .arg("--check")
        .arg(patch)
        .output()
        .unwrap_or_else(|e| panic!("check {}: {e}", patch.display()));
    if forward.status.success() {
        run(
            {
                let mut apply = command();
                apply.arg(patch);
                apply
            },
            "apply Ghostty backport",
        );
        return;
    }
    let reverse = command()
        .args(["--reverse", "--check"])
        .arg(patch)
        .output()
        .unwrap_or_else(|e| panic!("check applied {}: {e}", patch.display()));
    assert!(
        reverse.status.success(),
        "Ghostty patch {} neither applies nor is already applied in {}: {}",
        patch.display(),
        source.display(),
        String::from_utf8_lossy(&forward.stderr)
    );
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

#[cfg(test)]
mod patch_tests {
    use super::*;
    use std::env;

    #[test]
    fn patch_application_is_idempotent_for_checkouts_and_source_archives() {
        for repository in [false, true] {
            for crlf in [false, true] {
                let nonce = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let root = env::temp_dir()
                    .join(format!("morsh patch test {}-{nonce}", std::process::id()));
                let source = root.join("source with spaces");
                std::fs::create_dir_all(&source).unwrap();
                if repository {
                    run(
                        {
                            let mut init = Command::new("git");
                            init.args(["init", "--quiet"]).arg(&source);
                            init
                        },
                        "init test checkout",
                    );
                }
                let file = source.join("sample.txt");
                std::fs::write(&file, if crlf { "old\r\n" } else { "old\n" }).unwrap();
                let patch = root.join("backport.patch");
                std::fs::write(
                    &patch,
                    "--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-old\n+new\n",
                )
                .unwrap();
                apply_patches(&source, &root, false);
                apply_patches(&source, &root, false);
                assert!(!source.join(".ghostty-patches").exists());
                assert_eq!(std::fs::read_to_string(file).unwrap().trim(), "new");
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    /// The sample file's text with CRLF folded to LF. With `core.autocrlf`
    /// on (the default on Windows CI runners), `git checkout` restores files
    /// as CRLF and `git apply --ignore-whitespace` keeps that, so compare
    /// content rather than line endings.
    fn sample_text(source: &Path) -> String {
        std::fs::read_to_string(source.join("sample.txt"))
            .unwrap()
            .replace("\r\n", "\n")
    }

    #[test]
    fn changed_patch_set_refreshes_cached_checkout() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "morsh patch refresh {}-{nonce}",
            std::process::id()
        ));
        let source = root.join("source");
        let directory = root.join("patches");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            command.current_dir(&source).args(args);
            run(command, "prepare cached checkout");
        };
        git(&["init", "--quiet"]);
        std::fs::write(source.join("sample.txt"), "old\n").unwrap();
        git(&["add", "sample.txt"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ]);
        std::fs::write(source.join(".ghostty-commit"), "test").unwrap();
        let patch = directory.join("backport.patch");
        let write_patch = |new| {
            std::fs::write(
                &patch,
                format!("--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-old\n+{new}\n"),
            )
            .unwrap()
        };
        write_patch("first");
        // Migrate an already-patched checkout from the previous build script.
        apply_patch(&source, &patch);
        apply_patches(&source, &directory, true);
        let initial_stamp = std::fs::read(source.join(".ghostty-patches")).unwrap();
        apply_patches(&source, &directory, true);
        assert_eq!(
            std::fs::read(source.join(".ghostty-patches")).unwrap(),
            initial_stamp
        );
        assert_eq!(sample_text(&source), "first\n");
        write_patch("second");
        apply_patches(&source, &directory, true);
        assert_ne!(
            std::fs::read(source.join(".ghostty-patches")).unwrap(),
            initial_stamp
        );
        assert_eq!(sample_text(&source), "second\n");
        // Removing a patch must also discard its changes.
        std::fs::remove_file(patch).unwrap();
        apply_patches(&source, &directory, true);
        assert_eq!(sample_text(&source), "old\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
