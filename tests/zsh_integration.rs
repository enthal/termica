//! End-to-end test for the managed zsh bootstrap (Phase 3 / spec/03).
//!
//! Builds a [`ManagedSpawn`] for zsh, spawns the configured shell
//! inside a real PTY, writes the bootstrap to the PTY's slave-side
//! stdin as the first command (the "hidden first-command bootstrap"
//! mechanism described in spec/03), then drains the PTY looking for
//! the `ESC P Termica;{"type":"integration_ready",...} ESC \` DCS
//! sequence the bootstrap emits at the end.
//!
//! Skips silently if no zsh is installed.

#![forbid(unsafe_code)]
#![cfg(unix)]

use std::io::Read;
use std::time::{Duration, Instant};

use termica::integration::{ManagedSpawn, ShellSpec, managed_spawn_for};
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
#[ignore = "managed-spawn integration: hangs pending Phase 3F PTY-write timing fix"]
fn managed_zsh_emits_integration_ready_dcs() {
    let Some(zsh_path) = locate_zsh() else {
        eprintln!("skipping: no zsh installed on this runner");
        return;
    };

    let ManagedSpawn { argv, pty_bootstrap, env, wrapper_dir: _ } =
        managed_spawn_for(ShellSpec::Zsh, "test-session").expect("build managed spawn plan");
    assert!(pty_bootstrap.is_some(), "zsh spawn plan should carry a PTY bootstrap");

    // Translate argv[0] from the bare "zsh" the plan produces to an
    // absolute path the test runner's PTY layer can spawn. Everything
    // else (env, --no_rcs flag) stays as-is.
    let mut config = PtyConfig {
        program: zsh_path.into(),
        args: argv[1..].to_vec(),
        env: env.clone(),
        ..PtyConfig::default()
    };
    // Add an `exit` after our bootstrap so the shell terminates and
    // `drain_for` returns cleanly. We inject this at the very end of
    // the bootstrap content.
    let mut bootstrap = pty_bootstrap.expect("checked above");
    bootstrap.push_str("\nexit\n");
    config.args.extend(std::iter::empty::<String>());

    let mut session = PtySession::spawn(&config).expect("spawn zsh");

    // Write the bootstrap to the PTY's master end (which becomes the
    // shell's stdin). This is the "hidden first-command bootstrap"
    // mechanism: zsh starts with --no_rcs (no init files), reads our
    // bootstrap as its first interactive input, executes it, then
    // exits. In Termica proper the display would be suppressed during
    // this window; here we just read the bytes to verify they happen.
    session.write(bootstrap.as_bytes()).expect("write bootstrap to PTY");

    let reader = match session.take_reader() {
        Ok(r) => r,
        Err(PtyError::ReaderAlreadyTaken) => unreachable!(),
        Err(e) => panic!("take_reader: {e:?}"),
    };
    let bytes = drain_for(reader, Duration::from_secs(5));
    let s = String::from_utf8_lossy(&bytes);

    // Look for the DCS-JSON integration_ready envelope. The exact
    // payload includes the session ID, version, and shell name, all
    // of which we can assert on as substrings.
    let has_dcs_intro = s.contains("\x1bPTermica;");
    let has_integration_ready_type = s.contains("\"type\":\"integration_ready\"");
    let has_session_id = s.contains("\"session\":\"test-session\"");
    let has_zsh_shell = s.contains("\"shell\":\"zsh\"");
    let has_st = s.contains("\x1b\\");

    assert!(
        has_dcs_intro && has_integration_ready_type && has_session_id && has_zsh_shell && has_st,
        "expected a DCS-JSON integration_ready envelope; got bytes:\n{s:?}\n\
         (intro={has_dcs_intro}, type={has_integration_ready_type}, \
         session={has_session_id}, shell={has_zsh_shell}, st={has_st})"
    );
}
