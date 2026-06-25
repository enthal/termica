//! Integration test: the zsh bootstrap **drops its warm completion child**
//! after it goes idle, freeing the child's ~5 MB, and respawns it on the next
//! Tab ([spec/04a §"Zsh sidecar"](../spec/04a-completion.md)).
//!
//! The warm `zsh/zpty` completion child is spawned lazily on the first Tab and
//! reused. `__termica_zc_idle_check` (run from `termica_precmd`, i.e. once per
//! real prompt) releases it when `$SECONDS - last_use` exceeds the idle
//! threshold. These tests drive a real zsh, spawn the child via a completion
//! request, then force the clock forward and confirm the child is dropped —
//! while a still-fresh child is kept.
//!
//! Hermetic (`HOME` → tempdir, `ZDOTDIR` → a wrapper whose `.zshrc` IS our
//! bootstrap) and skips silently when zsh isn't installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

use termica::integration::ZSH_BOOTSTRAP;
use termica::submit_framing::completion_request_bytes_sentinel;

fn locate_zsh() -> Option<&'static str> {
    ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh", "/usr/local/bin/zsh"]
        .into_iter()
        .find(|c| std::path::Path::new(c).exists())
}

/// A completion request over a PIPE: the request bytes end in `\r` (a real tty
/// maps it to `\n`); a pipe doesn't, so swap the terminator for `\n`.
fn pipe_request(id: u64, line: &str) -> Vec<u8> {
    let mut req = completion_request_bytes_sentinel(id, line, true);
    *req.last_mut().expect("non-empty request") = b'\n';
    req
}

/// Run the bootstrap as a real zsh, feeding `lines` (each already terminated),
/// and return stdout. `None` ⇒ no zsh to run.
fn run_zsh(lines: &[Vec<u8>]) -> Option<String> {
    let zsh = locate_zsh()?;
    let home = tempfile::tempdir().expect("home tempdir");
    let wrapper = tempfile::tempdir().expect("zdotdir tempdir");
    std::fs::write(wrapper.path().join(".zshrc"), ZSH_BOOTSTRAP).expect("write .zshrc");
    std::fs::write(wrapper.path().join(".zshenv"), b"").expect("write .zshenv");

    let mut input = Vec::new();
    for l in lines {
        input.extend_from_slice(l);
    }

    let mut child = Command::new(zsh)
        .arg("-i")
        .env("HOME", home.path())
        .env("ZDOTDIR", wrapper.path())
        .env("TERMICA_SESSION_ID", "ztest")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn zsh");
    child.stdin.take().expect("stdin").write_all(&input).expect("write");
    let output = child.wait_with_output().expect("wait");
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Probe the live child: `zpty -t` returns 0 if alive, non-zero if not. Submit
/// it as one line so `$?` is exactly that test's result.
fn probe(tag: &str) -> Vec<u8> {
    format!("zpty -t __termica_zc; print -r -- \"{tag}=$?\"\n").into_bytes()
}

#[test]
fn zsh_idle_completion_child_is_dropped_after_going_idle() {
    // 1) a completion request spawns + stamps the warm child;
    // 2) jump `$SECONDS` far past the idle threshold;
    // 3) a real command fires precmd → `__termica_zc_idle_check` → drop;
    // 4) probe: the child must be gone.
    let Some(stdout) = run_zsh(&[
        pipe_request(1, "greet"),
        b"SECONDS=10000\n".to_vec(),
        probe("TZ_IDLE"),
        b"exit\n".to_vec(),
    ]) else {
        eprintln!("skipping: no zsh installed");
        return;
    };

    // The completion reply confirms the child actually spawned (precondition).
    assert!(
        stdout.contains(r#""type":"completion""#),
        "the completion request must spawn the warm child first; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("TZ_IDLE=1"),
        "an idle warm child is dropped (zpty -t fails → non-zero); stdout:\n{stdout}"
    );
}

#[test]
fn zsh_fresh_completion_child_is_kept() {
    // Same flow WITHOUT advancing the clock: a real command fires precmd, but
    // the child was just used, so it must stay alive.
    let Some(stdout) = run_zsh(&[
        pipe_request(1, "greet"),
        b"true\n".to_vec(),
        probe("TZ_FRESH"),
        b"exit\n".to_vec(),
    ]) else {
        eprintln!("skipping: no zsh installed");
        return;
    };

    assert!(
        stdout.contains(r#""type":"completion""#),
        "the completion request must spawn the warm child first; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("TZ_FRESH=0"),
        "a freshly-used warm child is kept alive (zpty -t succeeds → 0); stdout:\n{stdout}"
    );
}
