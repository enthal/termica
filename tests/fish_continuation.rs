//! Integration test: the fish bootstrap detects an **incomplete** command and
//! emits a `continuation` marker instead of executing — the fish equivalent of
//! bash/zsh `PS2` ([spec/03 §continuation](../spec/03-shell-integration.md),
//! [spec/04 §"Multi-line continuation"](../spec/04-prompt-editor.md)).
//!
//! fish has no `PS2` and no in-process completeness query, so the bootstrap
//! parse-checks each command with `fish -n` (no-execute). An EOF-while-
//! expecting-more error (trailing `&&`, open block, unbalanced quote) means
//! "incomplete" → emit `continuation` and loop without executing; anything
//! else runs. On the next submit Termica resends the WHOLE cumulative buffer
//! (fish keeps no partial state), which the bootstrap re-checks and, now
//! complete, executes.
//!
//! Hermetic (`--no-config`, `HOME` → tempdir) and skips silently when fish
//! isn't installed.

use std::io::Write;
use std::process::{Command, Stdio};

use termica::integration::FISH_BOOTSTRAP;
use termica::submit_framing::base64_encode;

fn locate(candidates: &[&str]) -> Option<String> {
    candidates.iter().find(|p| std::path::Path::new(p).exists()).map(|p| p.to_string())
}

fn spawn_fish_with(input: &[u8]) -> Option<String> {
    let fish = locate(&["/opt/homebrew/bin/fish", "/usr/local/bin/fish", "/usr/bin/fish"])?;
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut child = Command::new(&fish)
        .args(["--no-config", "-c", FISH_BOOTSTRAP])
        .env("HOME", tmp.path())
        .env("TERMICA_SESSION_ID", "test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fish");
    child.stdin.take().expect("stdin").write_all(input).expect("write");
    let output = child.wait_with_output().expect("wait");
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// One submission line: base64(command) + `\n` (a real tty uses `\r`+ICRNL;
/// over a pipe we terminate with `\n` so the bootstrap's `read` returns).
fn submit(cmd: &[u8]) -> Vec<u8> {
    let mut v = base64_encode(cmd).into_bytes();
    v.push(b'\n');
    v
}

#[test]
fn fish_incomplete_command_continues_then_the_whole_buffer_executes() {
    // Submit 1: an incomplete command (trailing `&&`). Submit 2: the WHOLE
    // cumulative buffer Termica resends for fish (continuation_to_send), now
    // complete. The bootstrap should emit exactly one `continuation` for the
    // first and execute the second.
    let mut input = Vec::new();
    input.extend_from_slice(&submit(b"echo HELLO &&"));
    input.extend_from_slice(&submit(b"echo HELLO &&\necho WORLD"));
    let Some(stdout) = spawn_fish_with(&input) else {
        eprintln!("skipping: no fish installed");
        return;
    };

    // Exactly one continuation — for the incomplete first submit.
    assert_eq!(
        stdout.matches(r#""type":"continuation""#).count(),
        1,
        "the incomplete submit emits one continuation marker; stdout:\n{stdout}"
    );
    // The incomplete submit is INERT: no preexec / command_finished fired for
    // it. Only the complete second submit runs, so exactly one of each.
    assert_eq!(
        stdout.matches(r#""type":"preexec""#).count(),
        1,
        "only the complete command is announced (incomplete submit is inert); stdout:\n{stdout}"
    );
    assert_eq!(
        stdout.matches(r#""type":"command_finished""#).count(),
        1,
        "only the complete command finishes; stdout:\n{stdout}"
    );
    // The complete command actually ran (the `&&` chain across the newline).
    assert!(
        stdout.contains("WORLD"),
        "the completed multi-line command executed; stdout:\n{stdout}"
    );
}

#[test]
fn fish_genuine_syntax_error_executes_rather_than_trapping() {
    // A genuine syntax error (not EOF-incomplete) must NOT be treated as a
    // continuation — otherwise the user is trapped adding lines that can never
    // complete it. The safe default is to execute and let fish show the error.
    let Some(stdout) = spawn_fish_with(&submit(b"echo )")) else {
        eprintln!("skipping: no fish installed");
        return;
    };

    assert_eq!(
        stdout.matches(r#""type":"continuation""#).count(),
        0,
        "a genuine syntax error is not a continuation; stdout:\n{stdout}"
    );
    assert_eq!(
        stdout.matches(r#""type":"preexec""#).count(),
        1,
        "the bad command still runs (so fish surfaces the error); stdout:\n{stdout}"
    );
}
