//! Regression test for the zsh bootstrap's login-shell profile sourcing
//! (spec/03, "Login shell").
//!
//! Termica spawns its managed zsh as a **login** shell (`zsh -i -l`) so
//! that `/etc/zprofile` runs `path_helper` (rebuilding PATH from
//! `/etc/paths` + `/etc/paths.d/*` — Homebrew, Go, cryptexes) exactly as a
//! normal macOS terminal does. But because Termica redirects `ZDOTDIR` at
//! the wrapper temp dir, zsh looks for `$ZDOTDIR/.zprofile` /
//! `$ZDOTDIR/.zlogin` (our wrapper, which has neither) and skips the
//! user's real ones — the same skip-and-recover problem already solved for
//! `~/.zshenv` and `~/.zshrc`. So the bootstrap sources the user's real
//! `~/.zprofile` (before `.zshrc`) and `~/.zlogin` (after), in login order.
//!
//! Without this, a GUI-launched Termica inherits launchd's bare PATH plus
//! whatever `~/.zshenv` adds, and silently loses every PATH entry the user
//! set in `~/.zprofile` (`brew shellenv`, `fish_add_path`-style prepends,
//! `~/.local/bin`).
//!
//! Hermetic: `zsh -f` skips all real system/user rc files, then we source
//! our bootstrap directly and assert it pulled in our fake profile files.
//! Skips silently if no zsh is installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use termica::integration::ZSH_BOOTSTRAP;

fn locate_zsh() -> Option<&'static str> {
    ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh", "/usr/local/bin/zsh"]
        .into_iter()
        .find(|c| std::path::Path::new(c).exists())
}

/// Materialise a wrapper dir exactly as Termica does, run the bootstrap
/// under a hermetic `$HOME` containing `extra_dotfiles`, and return the
/// captured value of `probe` (a shell expression) bracketed by sentinels.
fn run_bootstrap_capturing(probe: &str, home_dotfiles: &[(&str, &str)]) -> String {
    let zsh = locate_zsh().expect("caller checked zsh present");
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let wrapper = tmp.path().join("wrapper");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&wrapper).expect("wrapper");
    std::fs::write(wrapper.join(".zshrc"), ZSH_BOOTSTRAP).expect("write .zshrc");
    std::fs::write(wrapper.join(".zshenv"), "").expect("write .zshenv");
    for (name, body) in home_dotfiles {
        std::fs::write(home.join(name), body).expect("write home dotfile");
    }

    // Sentinels avoid a leading `:` so zsh can't mistake `$var:X` for a
    // history-style modifier (e.g. `:P` realpath) and eat the marker.
    let script = format!(
        r#"
        source "$ZDOTDIR/.zshrc"
        print -r -- "@@TZP_START@@{probe}@@TZP_END@@"
    "#
    );

    let output = std::process::Command::new(zsh)
        .args(["-f", "-c", &script])
        .env("HOME", &home)
        .env("ZDOTDIR", &wrapper)
        .output()
        .expect("run zsh");

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_once("@@TZP_START@@")
        .and_then(|(_, rest)| rest.split_once("@@TZP_END@@"))
        .map(|(val, _)| val.to_string())
        .unwrap_or_else(|| panic!("no sentinel in output; full stdout:\n{stdout}"))
}

#[test]
fn bootstrap_sources_user_zprofile() {
    if locate_zsh().is_none() {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    }
    // A `~/.zprofile` that exports a sentinel and prepends a sentinel dir to
    // PATH — the real-world shape (`brew shellenv`, `path_prepend`).
    let zprofile = "export TERMICA_PROFILE_RAN=1\nexport PATH=\"/sentinel/profile/bin:$PATH\"\n";
    let path = run_bootstrap_capturing("$PATH", &[(".zprofile", zprofile)]);
    assert!(
        path.contains("/sentinel/profile/bin"),
        "the bootstrap must source ~/.zprofile so login-only PATH entries survive.\n  PATH: {path}"
    );

    let ran = run_bootstrap_capturing("${TERMICA_PROFILE_RAN:-0}", &[(".zprofile", zprofile)]);
    assert_eq!(ran, "1", "~/.zprofile should have been sourced (TERMICA_PROFILE_RAN unset)");
}

#[test]
fn bootstrap_sources_user_zlogin_after_zshrc() {
    if locate_zsh().is_none() {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    }
    // `.zlogin` is sourced AFTER `.zshrc` in vanilla login zsh. Prove both
    // ran and the order held by having each append to a marker variable.
    let zshrc = "typeset -g TERMICA_ORDER=\"${TERMICA_ORDER}rc\"\n";
    let zlogin = "typeset -g TERMICA_ORDER=\"${TERMICA_ORDER}login\"\n";
    let order = run_bootstrap_capturing(
        "${TERMICA_ORDER:-none}",
        &[(".zshrc", zshrc), (".zlogin", zlogin)],
    );
    assert_eq!(order, "rclogin", "expected ~/.zshrc then ~/.zlogin (login order); got {order:?}");
}

#[test]
fn bootstrap_tolerates_absent_profile_files() {
    if locate_zsh().is_none() {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    }
    // No `.zprofile` / `.zlogin` in HOME: the bootstrap must not error and
    // must still reach the end (sentinel prints).
    let ok = run_bootstrap_capturing("ok", &[]);
    assert_eq!(ok, "ok", "bootstrap should complete cleanly with no profile files present");
}
