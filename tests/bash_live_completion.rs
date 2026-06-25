//! Integration test: the bash bootstrap answers a **live-shell completion
//! request** in-process, so a **runtime-defined** `complete -F` spec resolves
//! ([spec/03 §completion](../spec/03-shell-integration.md),
//! [spec/04a §"Bash sidecar"](../spec/04a-completion.md)).
//!
//! bash completion functions just fill `COMPREPLY` and need no readline
//! context, so — unlike zsh's captive child — the managed bash captures
//! completions IN-PROCESS. The request is dispatched as a guarded command
//! (`__termica_complete <id> <b64>`, no leading space) that the pane shell
//! runs. This test drives a real bash over a **PTY** (bash-preexec only
//! installs its hooks on a tty) and asserts the load-bearing properties:
//!
//! - a runtime-defined `complete -F` candidate completes;
//! - the request is **inert** to the mode machine — no `preexec` /
//!   `command_finished` marker fires for the sentinel (checked on the DCS
//!   marker stream, which excludes the tty's echo);
//! - it preserves the user's last `$?`.
//!
//! Hermetic (`HOME` → tempdir; the wrapper dir holds our bootstrap +
//! bash-preexec) and skips silently when a modern bash isn't installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::io::Read;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use termica::integration::{BASH_BOOTSTRAP, BASH_PREEXEC};
use termica::submit_framing::completion_request_bytes_sentinel;

/// A bash new enough for `bash-completion` 2.x (4.1+). macOS ships 3.2, so we
/// prefer Homebrew's. Returns the first that reports a 4+ major version.
fn locate_modern_bash() -> Option<String> {
    for cand in ["/opt/homebrew/bin/bash", "/usr/local/bin/bash", "/usr/bin/bash", "/bin/bash"] {
        if !std::path::Path::new(cand).exists() {
            continue;
        }
        let Ok(out) =
            std::process::Command::new(cand).arg("-c").arg("echo ${BASH_VERSINFO[0]}").output()
        else {
            continue;
        };
        let major: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
        if major >= 4 {
            return Some(cand.to_string());
        }
    }
    None
}

/// Extract every Termica DCS marker body (`ESC P Termica;<json> ESC \`) from a
/// raw PTY byte stream, so assertions see the MARKER stream only — never the
/// cooked tty's echo of the input (which `EchoSuppressor` strips in
/// production).
fn dcs_markers(raw: &[u8]) -> Vec<String> {
    const OPEN: &[u8] = b"\x1bPTermica;";
    const CLOSE: &[u8] = b"\x1b\\";
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = raw[i..].windows(OPEN.len()).position(|w| w == OPEN) {
        let start = i + rel + OPEN.len();
        let Some(end_rel) = raw[start..].windows(CLOSE.len()).position(|w| w == CLOSE) else {
            break;
        };
        out.push(String::from_utf8_lossy(&raw[start..start + end_rel]).into_owned());
        i = start + end_rel + CLOSE.len();
    }
    out
}

#[test]
fn bash_live_completion_sees_a_runtime_complete_spec_and_stays_inert() {
    let Some(bash) = locate_modern_bash() else {
        eprintln!("skipping: no bash >= 4 installed on this runner");
        return;
    };

    let home = tempfile::tempdir().expect("home tempdir");
    let wrapper = tempfile::tempdir().expect("wrapper tempdir");
    // The bootstrap sources bash-preexec.sh from alongside itself; write both.
    let bootstrap = wrapper.path().join("bootstrap.bash");
    std::fs::write(&bootstrap, BASH_BOOTSTRAP).expect("write bootstrap");
    std::fs::write(wrapper.path().join("bash-preexec.sh"), BASH_PREEXEC).expect("write preexec");

    let pair = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    // Match production argv (src/integration.rs): NO `--norc` (it would
    // override `--rcfile` on old bash), `--noediting` to disable readline.
    let mut cmd = CommandBuilder::new(&bash);
    cmd.args(["--noprofile", "--noediting", "--rcfile"]);
    cmd.arg(&bootstrap);
    cmd.arg("-i");
    cmd.env("HOME", home.path());
    cmd.env("TERMICA_SESSION_ID", "btest");
    // Keep the user's real bash-completion out of it; we test a runtime spec.
    cmd.cwd(home.path());

    let mut child = pair.slave.spawn_command(cmd).expect("spawn bash");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");

    // Drive the shell:
    //   1) register a runtime `complete -F` spec — the long tail a one-shot
    //      subprocess couldn't see;
    //   2) `false` so `$?` is 1 going into the request;
    //   3) the completion request for `frob be` (no leading space);
    //   4) print `$?` — must still be 1 if the sentinel preserved it;
    //   5) exit so the PTY drains.
    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(
        b"_frob() { COMPREPLY=( $(compgen -W 'alpha beta gamma' -- \"${COMP_WORDS[COMP_CWORD]}\") ); }\n",
    );
    input.extend_from_slice(b"complete -F _frob frob\n");
    input.extend_from_slice(b"false\n");
    input.extend_from_slice(&completion_request_bytes_sentinel(1, "frob be", false));
    input.extend_from_slice(b"echo \"TZ_STATUS=$?\"\n");
    input.extend_from_slice(b"exit\n");
    writer.write_all(&input).expect("write");
    drop(writer);

    // Read until the child exits (or a generous timeout).
    let mut raw = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
        if Instant::now() > deadline {
            break;
        }
    }
    let _ = child.wait();

    let markers = dcs_markers(&raw);
    let raw_str = String::from_utf8_lossy(&raw);

    // Exactly one completion reply, correlated to our request id, carrying the
    // runtime spec's candidate `beta` (from the `be` prefix).
    let completions: Vec<&String> =
        markers.iter().filter(|m| m.contains(r#""type":"completion""#)).collect();
    assert_eq!(completions.len(), 1, "one completion reply; markers:\n{markers:#?}");
    assert!(completions[0].contains(r#""id":1"#), "reply carries the id; {}", completions[0]);
    assert!(
        completions[0].contains(r#""beta""#),
        "the runtime `complete -F` candidate completes; {}",
        completions[0]
    );

    // INERTNESS: no preexec / command_finished marker may name the sentinel.
    assert!(
        !markers.iter().any(|m| m.contains("__termica_complete")),
        "the completion sentinel must be inert (no preexec/command_finished marker); markers:\n{markers:#?}"
    );
    // Sanity: the hooks DO fire for a real command (the `complete` line).
    assert!(
        markers
            .iter()
            .any(|m| m.contains(r#""type":"preexec""#) && m.contains("complete -F _frob")),
        "a real command still emits preexec; markers:\n{markers:#?}"
    );

    // `$?` survived the sentinel: 1 (from `false`) and still 1. The literal
    // appears in the command OUTPUT (`TZ_STATUS=1`), not the echo (`$?`).
    assert!(
        raw_str.contains("TZ_STATUS=1"),
        "the completion sentinel preserved the user's last exit status; raw:\n{raw_str}"
    );
}
