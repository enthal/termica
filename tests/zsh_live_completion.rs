//! Integration test: the zsh bootstrap answers a **live-shell completion
//! request** from a warm captive completion child, so a **runtime-defined**
//! alias completes ([spec/03 §completion](../spec/03-shell-integration.md),
//! [spec/04a §"Zsh sidecar"](../spec/04a-completion.md)).
//!
//! zsh has no `complete -C` and no Termica read-eval loop: the request is
//! dispatched as a guarded command (`__termica_complete <id> <b64>`) that
//! the pane shell runs, capturing matches from a persistent `zsh/zpty`
//! child driven by a real completion widget. This test exercises the WHOLE
//! path against a real zsh and asserts the load-bearing properties:
//!
//! - the runtime alias completes (a one-shot subprocess couldn't see it);
//! - the request is **inert** to the mode machine / block model — it emits
//!   no preexec / command_finished (the `__termica_complete` sentinel never
//!   appears in the marker stream);
//! - it preserves the user's last `$?`.
//!
//! Hermetic (`HOME` → tempdir, `ZDOTDIR` → a wrapper whose `.zshrc` IS our
//! bootstrap) and skips silently when zsh isn't installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::io::Write;
use std::process::{Command, Stdio};

use termica::integration::ZSH_BOOTSTRAP;
use termica::submit_framing::completion_request_bytes_zsh;

fn locate_zsh() -> Option<&'static str> {
    ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh", "/usr/local/bin/zsh"]
        .into_iter()
        .find(|c| std::path::Path::new(c).exists())
}

/// The zsh completion request over a PIPE: `completion_request_bytes_zsh`
/// ends in `\r` (a real tty maps it to `\n` via ICRNL); a pipe doesn't, and
/// a trailing `\r` would glue onto the base64 arg and break decoding — so
/// swap it for `\n`.
fn pipe_request(id: u64, line: &str) -> Vec<u8> {
    let mut req = completion_request_bytes_zsh(id, line);
    *req.last_mut().expect("non-empty request") = b'\n';
    req
}

#[test]
fn zsh_live_completion_sees_a_runtime_defined_alias_and_stays_inert() {
    let Some(zsh) = locate_zsh() else {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    };

    let home = tempfile::tempdir().expect("home tempdir");
    let wrapper = tempfile::tempdir().expect("zdotdir tempdir");
    // The wrapper's `.zshrc` IS our bootstrap (the ZDOTDIR redirect Termica
    // uses in production); an empty `.zshenv` keeps startup hermetic.
    std::fs::write(wrapper.path().join(".zshrc"), ZSH_BOOTSTRAP).expect("write .zshrc");
    std::fs::write(wrapper.path().join(".zshenv"), b"").expect("write .zshenv");

    // Feed the shell, line by line:
    //   1) define an alias AT RUNTIME (only the live shell knows it),
    //   2) `false` so `$?` is 1 going into the request,
    //   3) the completion request for `greet`,
    //   4) print `$?` — must still be 1 if the sentinel preserved it,
    //   5) exit so the pipe drains and the child is reaped.
    let mut input = Vec::new();
    input.extend_from_slice(b"alias greethere='echo hi'\n");
    input.extend_from_slice(b"false\n");
    input.extend_from_slice(&pipe_request(1, "greet"));
    input.extend_from_slice(b"print -r -- \"TZ_STATUS=$?\"\n");
    input.extend_from_slice(b"exit\n");

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
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Exactly one completion reply, correlated to our request id.
    let completions = stdout.matches(r#""type":"completion""#).count();
    assert_eq!(completions, 1, "one completion reply per request; stdout:\n{stdout}");
    assert!(stdout.contains(r#""id":1"#), "reply carries the request id; stdout:\n{stdout}");

    // The runtime alias `greethere` is in the candidates — the payoff. zsh
    // v1 emits values only, so it appears as a bare value array element.
    assert!(
        stdout.contains(r#""greethere""#),
        "the runtime-defined alias `greethere` completes via the live shell; stdout:\n{stdout}"
    );

    // INERTNESS: the sentinel must never run as an observable command. If a
    // preexec/command_finished had fired for it, the literal sentinel name
    // would appear in the marker stream — assert it never does.
    assert!(
        !stdout.contains("__termica_complete"),
        "the completion sentinel must be inert (no preexec/command_finished, no echo); stdout:\n{stdout}"
    );
    // Sanity: the hooks DO work for a real command (the alias was announced).
    assert!(
        stdout.contains(r#""type":"preexec""#) && stdout.contains("alias greethere"),
        "a real command still emits preexec; stdout:\n{stdout}"
    );

    // `$?` survived the sentinel: it was 1 (from `false`) and still is.
    assert!(
        stdout.contains("TZ_STATUS=1"),
        "the completion sentinel preserved the user's last exit status; stdout:\n{stdout}"
    );
}
