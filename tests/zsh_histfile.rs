//! Regression test for the zsh bootstrap's `HISTFILE` repair (spec/03).
//!
//! Termica spawns zsh with `ZDOTDIR=<wrapper-temp-dir>` so zsh sources
//! our bootstrap as its `.zshrc`. But files sourced BEFORE that — most
//! notably macOS's `/etc/zshrc`, which does
//! `HISTFILE=${ZDOTDIR:-$HOME}/.zsh_history` — run while `ZDOTDIR` still
//! points at the wrapper, pinning `HISTFILE` inside a throwaway temp dir.
//! Uncorrected, the shell's native history (up-arrow, `!!`, `fc`) reads
//! and writes that temp file instead of the user's real `~/.zsh_history`,
//! and Termica deletes it on pane close — silent history loss.
//!
//! The bootstrap repairs this: if `HISTFILE` points inside our wrapper
//! `ZDOTDIR`, it's rebased onto the user's real config home. This test
//! reproduces the precondition hermetically (with `zsh -f` so no real
//! system/user rc files interfere) and asserts the rebase.
//!
//! Skips silently if no zsh is installed (e.g. a Linux CI runner without
//! zsh); runs on any machine that has it, including macOS CI.

#![forbid(unsafe_code)]
#![cfg(unix)]

use termica::integration::ZSH_BOOTSTRAP;

fn locate_zsh() -> Option<&'static str> {
    ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh", "/usr/local/bin/zsh"]
        .into_iter()
        .find(|c| std::path::Path::new(c).exists())
}

#[test]
fn bootstrap_rebases_histfile_out_of_wrapper_to_home() {
    let Some(zsh) = locate_zsh() else {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let wrapper = tmp.path().join("wrapper");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&wrapper).expect("wrapper");
    // Materialise the wrapper exactly as Termica does: our bootstrap as
    // `.zshrc`, an empty `.zshenv`.
    std::fs::write(wrapper.join(".zshrc"), ZSH_BOOTSTRAP).expect("write .zshrc");
    std::fs::write(wrapper.join(".zshenv"), "").expect("write .zshenv");

    // `-f` (NO_RCS) skips every real system/user startup file, so the
    // test is hermetic and deterministic on any runner. We then
    // reproduce the exact precondition the bug needs — `HISTFILE` pinned
    // inside the wrapper `ZDOTDIR`, as macOS `/etc/zshrc` would do — and
    // source our bootstrap, which must rebase it under `$HOME`.
    // Wrap the value in sentinels: the bootstrap emits a DCS sequence
    // with no trailing newline, so the `print` output shares a line with
    // those escape bytes — sentinels let us isolate the value cleanly.
    let script = r#"
        HISTFILE="$ZDOTDIR/.zsh_history"
        source "$ZDOTDIR/.zshrc"
        print -r -- "<<<HIST:$HISTFILE:HIST>>>"
    "#;

    let output = std::process::Command::new(zsh)
        .args(["-f", "-c", script])
        .env("HOME", &home)
        .env("ZDOTDIR", &wrapper)
        .env_remove("HISTFILE")
        .output()
        .expect("run zsh");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let got = stdout
        .split_once("<<<HIST:")
        .and_then(|(_, rest)| rest.split_once(":HIST>>>"))
        .map(|(val, _)| val)
        .unwrap_or_else(|| panic!("no HIST sentinel in output; full stdout:\n{stdout}"));

    let expected = home.join(".zsh_history");
    let expected = expected.to_str().expect("utf8 path");
    assert_eq!(
        got, expected,
        "HISTFILE must be rebased onto the user's real config home, not left inside the \
         wrapper temp dir.\n  got:      {got}\n  expected: {expected}\nfull stdout:\n{stdout}"
    );
    assert!(!got.contains("wrapper"), "HISTFILE still points inside the wrapper temp dir: {got}");
}

#[test]
fn bootstrap_leaves_histfile_outside_wrapper_untouched() {
    // The rebase is conditional: a HISTFILE that does NOT point inside our
    // wrapper (e.g. one the user set elsewhere, or a vanilla
    // `$HOME/.zsh_history`) must be left exactly as-is. Guards against an
    // over-eager repair that would clobber a deliberate user setting.
    let Some(zsh) = locate_zsh() else {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let wrapper = tmp.path().join("wrapper");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&wrapper).expect("wrapper");
    std::fs::write(wrapper.join(".zshrc"), ZSH_BOOTSTRAP).expect("write .zshrc");
    std::fs::write(wrapper.join(".zshenv"), "").expect("write .zshenv");

    let custom = home.join(".my_custom_history");
    let custom = custom.to_str().expect("utf8 path");
    // HISTFILE set to a path with no relation to the wrapper.
    let script = format!(
        r#"
        HISTFILE="{custom}"
        source "$ZDOTDIR/.zshrc"
        print -r -- "<<<HIST:$HISTFILE:HIST>>>"
    "#
    );

    let output = std::process::Command::new(zsh)
        .args(["-f", "-c", &script])
        .env("HOME", &home)
        .env("ZDOTDIR", &wrapper)
        .env_remove("HISTFILE")
        .output()
        .expect("run zsh");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let got = stdout
        .split_once("<<<HIST:")
        .and_then(|(_, rest)| rest.split_once(":HIST>>>"))
        .map(|(val, _)| val)
        .unwrap_or_else(|| panic!("no HIST sentinel in output; full stdout:\n{stdout}"));

    assert_eq!(
        got, custom,
        "a HISTFILE set outside the wrapper must be left untouched.\n  \
         got:      {got}\n  expected: {custom}\nfull stdout:\n{stdout}"
    );
}
