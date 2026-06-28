//! Integration test: the bash sidecar reports its `COMP_WORDBREAKS` current
//! word (`cur`) in the completion reply, so Termica can re-base the accept
//! replace range onto it ([spec/04a §"Bash sidecar"](../spec/04a-completion.md),
//! [#183](https://github.com/enthal/termica/issues/183)).
//!
//! bash splits `COMP_WORDS` on `$COMP_WORDBREAKS` (`=`, `:`, …), so for
//! `d FOO=ba` the completion function completes `cur="ba"`, and bash replaces
//! only that word. Termica's editor token is the wider `FOO=ba`; without the
//! reported `cur` it would replace the whole token. This drives a real bash over
//! a PTY and asserts the emitted `cur` is the post-word-break word.
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

fn completion_for_id(markers: &[String], id: u64) -> String {
    let needle = format!(r#""id":{id}"#);
    markers
        .iter()
        .find(|m| m.contains(r#""type":"completion""#) && m.contains(&needle))
        .unwrap_or_else(|| panic!("no completion reply for id {id}; markers:\n{markers:#?}"))
        .clone()
}

#[test]
fn bash_completion_reports_comp_wordbreaks_cur() {
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
    cmd.env("TERMICA_SESSION_ID", "cur");
    cmd.cwd(home.path());

    let mut child = pair.slave.spawn_command(cmd).expect("spawn bash");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");

    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(b"_d() { COMPREPLY=( \"cur=[${COMP_WORDS[COMP_CWORD]}]\" ); }\n");
    input.extend_from_slice(b"complete -F _d d\n");
    // id 1: `=` word-break → cur is `ba`. id 2: no word-break → cur is `che`.
    input.extend_from_slice(&completion_request_bytes_sentinel(1, "d FOO=ba", false));
    input.extend_from_slice(&completion_request_bytes_sentinel(2, "d che", false));
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

    // `d FOO=ba` → the `=` is a COMP_WORDBREAKS char, so cur is `ba` (the word
    // after the break), NOT the whole `FOO=ba`.
    let m1 = completion_for_id(&markers, 1);
    assert!(m1.contains(r#""cur":"ba""#), "cur is the post-`=` word `ba`; got: {m1}");

    // `d che` → no word-break, so cur is the whole `che`.
    let m2 = completion_for_id(&markers, 2);
    assert!(m2.contains(r#""cur":"che""#), "cur is the whole word `che`; got: {m2}");
}
