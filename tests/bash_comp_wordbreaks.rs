//! Integration test: the bash sidecar splits the completion line the way
//! readline does — on whitespace AND on `$COMP_WORDBREAKS` characters (`=`,
//! `:`, …) — so a completion function sees the right `cur`
//! ([spec/04a §"Bash sidecar"](../spec/04a-completion.md)).
//!
//! The naive `read -ra` split kept `FOO=ba` as one word, so `export FOO=ba`<Tab>
//! handed the function `cur="FOO=ba"` instead of `"ba"`. This drives a real
//! bash over a PTY with a `complete -F` function that echoes the `cur`/`prev`
//! it was handed, and asserts the `=` boundary was honoured.
//!
//! Skips silently when no bash >= 4 is installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::io::Read;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use termica::integration::{BASH_BOOTSTRAP, BASH_PREEXEC};
use termica::submit_framing::completion_request_bytes_sentinel;

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
        if String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().unwrap_or(0) >= 4 {
            return Some(cand.to_string());
        }
    }
    None
}

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
fn bash_completion_splits_on_comp_wordbreaks() {
    let Some(bash) = locate_modern_bash() else {
        eprintln!("skipping: no bash >= 4 installed on this runner");
        return;
    };

    let home = tempfile::tempdir().expect("home");
    let wrapper = tempfile::tempdir().expect("wrapper");
    let bootstrap = wrapper.path().join("bootstrap.bash");
    std::fs::write(&bootstrap, BASH_BOOTSTRAP).expect("write bootstrap");
    std::fs::write(wrapper.path().join("bash-preexec.sh"), BASH_PREEXEC).expect("write preexec");

    let pair = native_pty_system()
        .openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(&bash);
    cmd.args(["--noprofile", "--noediting", "--rcfile"]);
    cmd.arg(&bootstrap);
    cmd.arg("-i");
    cmd.env("HOME", home.path());
    cmd.env("TERMICA_SESSION_ID", "wb");
    cmd.cwd(home.path());

    let mut child = pair.slave.spawn_command(cmd).expect("spawn bash");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");

    // A completion that reports the `cur` ($2) it was handed, so we can assert
    // the line was split on `=`. Then a completion request for `wb FOO=ba`.
    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(b"_wb() { COMPREPLY=( \"got_cur=[$2]\" ); }\n");
    input.extend_from_slice(b"complete -F _wb wb\n");
    input.extend_from_slice(&completion_request_bytes_sentinel(1, "wb FOO=ba", false));
    input.extend_from_slice(b"exit\n");
    writer.write_all(&input).expect("write");
    drop(writer);

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
    let completion =
        markers.iter().find(|m| m.contains(r#""type":"completion""#)).unwrap_or_else(|| {
            panic!("no completion reply; markers:\n{markers:#?}");
        });
    // readline-faithful: the `=` is a word break, so `cur` is `ba`, not `FOO=ba`.
    assert!(
        completion.contains("got_cur=[ba]"),
        "the `=` COMP_WORDBREAKS boundary is honoured (cur=ba); got: {completion}"
    );
    assert!(
        !completion.contains("got_cur=[FOO=ba]"),
        "the naive whitespace-only split (cur=FOO=ba) must be gone; got: {completion}"
    );
}
