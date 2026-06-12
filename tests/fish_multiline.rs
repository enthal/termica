//! Integration test: the fish bootstrap's read-eval loop runs a
//! **base64-framed multi-line command as a single unit**, not split
//! line-by-line ([spec/03 §"fish"](../spec/03-shell-integration.md)).
//!
//! Hermetic (`--no-config`, `HOME` → tempdir) and skips silently when fish
//! isn't installed, matching the other shell-integration tests.

use std::io::Write;
use std::process::{Command, Stdio};

use termica::integration::FISH_BOOTSTRAP;
use termica::submit_framing::base64_encode;

fn locate(candidates: &[&str]) -> Option<String> {
    candidates.iter().find(|p| std::path::Path::new(p).exists()).map(|p| p.to_string())
}

#[test]
fn fish_runs_base64_multiline_command_as_one_unit() {
    let Some(fish) = locate(&["/opt/homebrew/bin/fish", "/usr/local/bin/fish", "/usr/bin/fish"])
    else {
        eprintln!("skipping: no fish installed");
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");

    // Two commands; the second is a multi-line `for` block. Termica frames
    // each as base64 on one line; over a pipe we terminate with `\n` (a
    // real tty would use `\r` + ICRNL). Closing stdin ends the loop.
    let commands = ["echo FIRST", "for i in 1 2\necho LOOP$i\nend"];
    let mut input = String::new();
    for cmd in commands {
        input.push_str(&base64_encode(cmd.as_bytes()));
        input.push('\n');
    }

    let mut child = Command::new(&fish)
        .args(["--no-config", "-c", FISH_BOOTSTRAP])
        .env("HOME", tmp.path())
        .env("TERMICA_SESSION_ID", "test")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fish");
    child.stdin.take().expect("stdin").write_all(input.as_bytes()).expect("write");
    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The key assertion: exactly one `command_finished` per input command.
    // If the multi-line `for` were split line-by-line (the bug), the
    // incomplete `for i in 1 2` / `end` fragments would each run and finish,
    // producing MORE than two.
    let finished = stdout.matches(r#""type":"command_finished""#).count();
    assert_eq!(
        finished, 2,
        "each command must run once; the multi-line for-loop must not be split.\nstdout:\n{stdout}"
    );

    // And the for-loop actually executed as one block.
    assert!(stdout.contains("FIRST"), "first command ran; stdout:\n{stdout}");
    assert!(
        stdout.contains("LOOP1") && stdout.contains("LOOP2"),
        "the multi-line for-loop iterated; stdout:\n{stdout}"
    );

    // The preexec marker must PRESERVE the newlines (as escaped `\n`), so
    // history records the multi-line command faithfully rather than
    // flattening it to spaces. (`\\n` here is a literal backslash-n.)
    assert!(
        stdout.contains("for i in 1 2\\necho LOOP$i\\nend"),
        "preexec must keep the command's newlines escaped, not collapse them; stdout:\n{stdout}"
    );
}
