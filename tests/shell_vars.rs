//! Integration tests for the `shell_vars` probe (spec/03).
//!
//! Each shell's bootstrap defines a `termica_emit_vars` helper that the
//! precmd hook calls: it emits the NAMES of the shell's current variables
//! (parameters) — never their values — as a `shell_vars` DCS-JSON message,
//! change-gated so a steady prompt loop costs almost nothing. This feeds
//! Termica's `$VAR` tab-completion so it reflects the LIVE shell,
//! including non-exported parameters like `HISTFILE` that the spawn-time
//! environment snapshot can't see.
//!
//! These tests source the real bootstrap and invoke the emit helper
//! directly (a real precmd cycle needs an interactive prompt, which is
//! awkward to drive in `-c`), asserting the emitted payload. They are
//! hermetic and skip silently when the shell isn't installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use termica::integration::{BASH_BOOTSTRAP, BASH_PREEXEC, FISH_BOOTSTRAP, ZSH_BOOTSTRAP};

fn locate(candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|c| std::path::Path::new(c).exists())
}

#[test]
fn zsh_emit_vars_reports_names_not_values() {
    let Some(zsh) = locate(&["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh"]) else {
        eprintln!("skipping: no zsh installed");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let boot = tmp.path().join("bootstrap.zsh");
    std::fs::write(&boot, ZSH_BOOTSTRAP).expect("write bootstrap");
    let boot = boot.to_str().expect("utf8");

    // `-f` (NO_RCS) → hermetic. Set a non-exported parameter (HISTFILE,
    // the bug that motivated this) and an exported var whose VALUE is a
    // sentinel secret, then source the bootstrap and emit. The names must
    // appear; the secret value must NOT (names-only invariant).
    let script = format!(
        r#"
        HISTFILE="$HOME/.zsh_history"
        export TERMICA_TEST_SECRET="s3cr3t-do-not-leak"
        source "{boot}"
        termica_emit_vars
    "#
    );

    let output = std::process::Command::new(zsh)
        .args(["-f", "-c", &script])
        .env("HOME", tmp.path())
        .output()
        .expect("run zsh");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Isolate the shell_vars DCS payload.
    let payload = stdout
        .split_once(r#""type":"shell_vars""#)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("no shell_vars message emitted; stdout:\n{stdout}"));

    assert!(
        payload.contains("\"HISTFILE\""),
        "HISTFILE (a non-exported parameter) should be reported; stdout:\n{stdout}"
    );
    assert!(
        payload.contains("\"HOME\"") && payload.contains("\"TERMICA_TEST_SECRET\""),
        "exported names should be reported too; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("s3cr3t-do-not-leak"),
        "VALUES must never be emitted — only names; stdout:\n{stdout}"
    );
}

#[test]
fn bash_emit_vars_reports_names_not_values() {
    let Some(bash) = locate(&["/bin/bash", "/usr/bin/bash", "/opt/homebrew/bin/bash"]) else {
        eprintln!("skipping: no bash installed");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let boot = tmp.path().join("bootstrap.bash");
    // bash bootstrap sources bash-preexec.sh from alongside itself.
    std::fs::write(&boot, BASH_BOOTSTRAP).expect("write bootstrap");
    std::fs::write(tmp.path().join("bash-preexec.sh"), BASH_PREEXEC).expect("write preexec");
    let boot = boot.to_str().expect("utf8");

    let script = format!(
        r#"
        export TERMICA_TEST_SECRET="s3cr3t-do-not-leak"
        source "{boot}"
        termica_emit_vars
    "#
    );
    let output = std::process::Command::new(bash)
        .args(["--norc", "--noprofile", "-c", &script])
        .env("HOME", tmp.path())
        .output()
        .expect("run bash");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let payload = stdout
        .split_once(r#""type":"shell_vars""#)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("no shell_vars message emitted; stdout:\n{stdout}"));
    assert!(
        payload.contains("\"TERMICA_TEST_SECRET\"") && payload.contains("\"HOME\""),
        "variable names should be reported; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("s3cr3t-do-not-leak"),
        "VALUES must never be emitted — only names; stdout:\n{stdout}"
    );
}

#[test]
fn fish_emit_vars_reports_names_not_values() {
    let Some(fish) = locate(&["/usr/bin/fish", "/opt/homebrew/bin/fish", "/usr/local/bin/fish"])
    else {
        eprintln!("skipping: no fish installed");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let boot = tmp.path().join("bootstrap.fish");
    std::fs::write(&boot, FISH_BOOTSTRAP).expect("write bootstrap");
    let boot = boot.to_str().expect("utf8");

    // `--no-config` keeps it hermetic; source the bootstrap functions and
    // invoke the emitter.
    let script =
        format!("set -gx TERMICA_TEST_SECRET s3cr3t-do-not-leak; source {boot}; termica_emit_vars");
    let output = std::process::Command::new(fish)
        .args(["--no-config", "-c", &script])
        .env("HOME", tmp.path())
        .output()
        .expect("run fish");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let payload = stdout
        .split_once(r#""type":"shell_vars""#)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("no shell_vars message emitted; stdout:\n{stdout}"));
    assert!(
        payload.contains("\"TERMICA_TEST_SECRET\""),
        "variable names should be reported; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("s3cr3t-do-not-leak"),
        "VALUES must never be emitted — only names; stdout:\n{stdout}"
    );
}
