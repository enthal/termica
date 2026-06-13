//! Integration test: the fish bootstrap answers a **live-shell completion
//! request** from its own running process, so a **runtime-defined** alias
//! completes ([spec/03 §completion](../spec/03-shell-integration.md),
//! [spec/04a §"Fish sidecar"](../spec/04a-completion.md)).
//!
//! This is the whole point of live-shell completion: an alias the user
//! typed *at the prompt* lives only in the pane's own fish process, so a
//! fresh `fish -c 'complete -C …'` subprocess can't see it — but a request
//! routed to the live shell can.
//!
//! Hermetic (`--no-config`, `HOME` → tempdir) and skips silently when fish
//! isn't installed, matching the other shell-integration tests.

use std::io::Write;
use std::process::{Command, Stdio};

use termica::integration::FISH_BOOTSTRAP;
use termica::submit_framing::{base64_encode, completion_request_bytes};

fn locate(candidates: &[&str]) -> Option<String> {
    candidates.iter().find(|p| std::path::Path::new(p).exists()).map(|p| p.to_string())
}

#[test]
fn fish_live_completion_sees_a_runtime_defined_alias() {
    let Some(fish) = locate(&["/opt/homebrew/bin/fish", "/usr/local/bin/fish", "/usr/bin/fish"])
    else {
        eprintln!("skipping: no fish installed");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");

    // 1) A command that defines an alias AT RUNTIME (base64-framed, like a
    //    real submission). 2) A completion request for `hel`. Over a pipe
    //    we terminate each line with `\n` (a real tty uses `\r`+ICRNL).
    let mut input = Vec::new();
    input.extend_from_slice(base64_encode(b"alias hello='echo HI'").as_bytes());
    input.push(b'\n');
    input.extend_from_slice(&completion_request_bytes(1, "hel"));
    // completion_request_bytes ends in `\r`; over the pipe add `\n` so
    // `read` returns the line (no tty to map CR→NL here).
    input.push(b'\n');

    let mut child = Command::new(&fish)
        .args(["--no-config", "-c", FISH_BOOTSTRAP])
        .env("HOME", tmp.path())
        .env("TERMICA_SESSION_ID", "test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fish");
    child.stdin.take().expect("stdin").write_all(&input).expect("write");
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Exactly one completion reply, correlated to our request id.
    let completions = stdout.matches(r#""type":"completion""#).count();
    assert_eq!(completions, 1, "one completion reply per request; stdout:\n{stdout}");
    assert!(stdout.contains(r#""id":1"#), "reply carries the request id; stdout:\n{stdout}");

    // The runtime alias `hello` is in the candidates — the payoff: a fresh
    // `fish -c` subprocess would NOT see it, only the live shell does. The
    // reply carries the raw `complete -C` lines, so the candidate array
    // leads with the `hello` line (value\tdescription).
    assert!(
        stdout.contains(r#""value":["hello"#),
        "the runtime-defined alias `hello` completes via the live shell; stdout:\n{stdout}"
    );

    // A completion request must NOT run as a command: it goes through the
    // completion branch, never the eval branch. So only the alias-defining
    // command emits a preexec + command_finished; the completion request
    // emits neither. (If the request HAD been eval'd, the base64-garbage
    // `complete\t1\t…` would have produced a second preexec/command_finished.)
    let preexecs = stdout.matches(r#""type":"preexec""#).count();
    let finished = stdout.matches(r#""type":"command_finished""#).count();
    assert_eq!(preexecs, 1, "only the alias command is announced; stdout:\n{stdout}");
    assert_eq!(
        finished, 1,
        "only the alias command finishes; the completion request does not execute; stdout:\n{stdout}"
    );
}
