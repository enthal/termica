//! End-to-end test for the auto-installed zsh OSC 7 integration.
//!
//! Spawns a real zsh through `PtySession`, lets the ZDOTDIR override
//! source our hook, has the shell `cd` to a known directory, and
//! drains the PTY reader looking for the OSC 7 escape sequence with
//! the right path payload.
//!
//! Gated on `cfg(unix)` and on the presence of `/bin/zsh` — if zsh
//! isn't installed, the test prints a skip notice rather than
//! failing, so this file is safe to land before all CI runners have
//! zsh (it lives behind `which zsh` at runtime).

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::io::Read;
use std::time::{Duration, Instant};

use termica::pty::{PtyConfig, PtyError, PtySession};

fn drain_for(reader: Box<dyn Read + Send>, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let mut reader = reader;
    while Instant::now() < deadline {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    out
}

fn locate_zsh() -> Option<&'static str> {
    ["/bin/zsh", "/usr/bin/zsh", "/opt/homebrew/bin/zsh", "/usr/local/bin/zsh"]
        .into_iter()
        .find(|c| std::path::Path::new(c).exists())
}

#[test]
fn zsh_emits_osc7_after_cd_via_termica_zdotdir_hook() {
    let Some(zsh) = locate_zsh() else {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    };

    // `-i` keeps it an interactive shell so our `.zshrc` runs (and
    // therefore the OSC 7 hook is installed). We then `cd` to
    // `/tmp` (which exists on every unix runner) and `exit` so the
    // shell terminates cleanly and `drain_for` returns.
    let mut session = PtySession::spawn(&PtyConfig {
        program: zsh.into(),
        args: vec!["-i".into(), "-c".into(), "cd /tmp; exit".into()],
        ..PtyConfig::default()
    })
    .expect("spawn zsh");

    let reader = match session.take_reader() {
        Ok(r) => r,
        Err(PtyError::ReaderAlreadyTaken) => unreachable!(),
        Err(e) => panic!("take_reader: {e:?}"),
    };
    let bytes = drain_for(reader, Duration::from_secs(5));
    let s = String::from_utf8_lossy(&bytes);

    // Two OSC 7 sequences should reach us: one at shell startup
    // (initial cwd) and one after `cd /tmp` (chpwd hook). We assert
    // the second by looking for the `/tmp` payload.
    let has_osc7 = s.contains("\x1b]7;file://");
    let has_tmp_payload = s.contains("/tmp\x1b\\") || s.contains("/tmp\x07");
    assert!(
        has_osc7 && has_tmp_payload,
        "expected an OSC 7 sequence carrying /tmp; got bytes:\n{s:?}\n\
         (has_osc7={has_osc7}, has_tmp_payload={has_tmp_payload})"
    );
}
