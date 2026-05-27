//! Managed shell-integration wrappers.
//!
//! Per [spec/03](../spec/03-shell-integration.md), Termica spawns
//! its own managed shells with rc-file loading disabled, then
//! delivers a Termica-controlled bootstrap that sources the user's
//! real dotfiles and installs Termica's lifecycle hooks.
//!
//! Mechanism per shell:
//!
//! - **zsh** — `zsh -i` with `ZDOTDIR=<termica-wrapper-dir>` in env.
//!   The wrapper's `.zshrc` immediately `unset ZDOTDIR`'s, sources
//!   `~/.zshenv`, re-evaluates ZDOTDIR, sources the user's real
//!   `.zshrc`, then installs our hooks.
//! - **bash** — `bash --noprofile --norc --rcfile <wrapper> -i`.
//!   The wrapper rcfile is generated on disk; bash reads it during
//!   normal startup.
//! - **fish** — `fish --no-config --init-command "<bootstrap>" -i`.
//!   fish executes the init-command before the first prompt.
//!
//! Opt out by setting `TERMICA_NO_SHELL_INTEGRATION=1`; spawned
//! shells skip the managed-startup machinery and launch with normal
//! rc-file processing.

#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::Path;

use tempfile::TempDir;

/// Bootstrap .zshrc for zsh. Materialised under Termica's data
/// directory; zsh is spawned with `ZDOTDIR=<that-dir>` so this file
/// becomes its .zshrc.
pub const ZSH_BOOTSTRAP: &str = include_str!("../integration/zsh-bootstrap.zsh");

/// Bootstrap wrapper for bash. Written to disk and passed via
/// `bash --rcfile <path>`.
pub const BASH_BOOTSTRAP: &str = include_str!("../integration/bash-bootstrap.bash");

/// Vendored bash-preexec.sh (MIT-licensed, from rcaloras/bash-preexec).
/// Sourced by the bash wrapper.
pub const BASH_PREEXEC: &str = include_str!("../integration/bash-preexec.sh");

/// Bootstrap script for fish. Passed inline via `--init-command`.
pub const FISH_BOOTSTRAP: &str = include_str!("../integration/fish-bootstrap.fish");

/// Integration protocol version this build emits / accepts.
/// Matches `SUPPORTED_PROTOCOL_VERSION` in [`crate::shell`].
pub const INTEGRATION_VERSION: u32 = 1;

/// Which shell to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSpec {
    Zsh,
    Bash,
    Fish,
}

impl ShellSpec {
    /// Resolve a shell from a `$SHELL`-style path by looking at the
    /// basename. Unknown shells default to `Zsh` (the most common on
    /// macOS) — callers can override.
    pub fn from_shell_path(path: &str) -> Self {
        let basename = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or("");
        match basename {
            "bash" => ShellSpec::Bash,
            "fish" => ShellSpec::Fish,
            _ => ShellSpec::Zsh,
        }
    }

    /// Human-readable name used in env vars and diagnostics.
    pub fn name(&self) -> &'static str {
        match self {
            ShellSpec::Zsh => "zsh",
            ShellSpec::Bash => "bash",
            ShellSpec::Fish => "fish",
        }
    }
}

/// A complete prepared shell-spawn plan. Materialised once per pane
/// spawn; consumed by the PTY-spawn code in [`crate::pane`].
#[derive(Debug)]
pub struct ManagedSpawn {
    /// argv to spawn. argv[0] is the program; subsequent entries
    /// are flags.
    pub argv: Vec<String>,
    /// If `Some`, the bootstrap content to write to the PTY as the
    /// first command after spawn. Reserved for future use; all
    /// current shells (zsh / bash / fish) deliver the bootstrap via
    /// CLI / env-var mechanisms instead.
    pub pty_bootstrap: Option<String>,
    /// Environment variables to set on the spawned process. Caller
    /// is expected to merge these with the inherited env.
    pub env: Vec<(String, String)>,
    /// Owned per-spawn temp directory holding the wrapper file(s)
    /// the shell will read at startup (zsh: `$ZDOTDIR/.zshrc` and
    /// `.zshenv`; bash: `bash-bootstrap.bash` + `bash-preexec.sh`;
    /// fish: none, because `--init-command` is inline).
    ///
    /// The directory's lifetime is tied to whatever owns this
    /// `ManagedSpawn` (in production: stashed inside `PaneSession`).
    /// When that owner is dropped, the `TempDir` Drop deletes the
    /// directory recursively — so the wrapper files exist exactly
    /// as long as the pane that needs them.
    pub wrapper_dir: Option<TempDir>,
}

/// Build a [`ManagedSpawn`] for the given shell.
///
/// `session_id` is the Termica session ID exposed as
/// `TERMICA_SESSION_ID`. The integration script echoes it back in
/// every lifecycle message so stale messages from a child that
/// lingered after restart can be discriminated.
///
/// Wrapper files (zsh's `.zshrc`, bash's rcfile + `bash-preexec.sh`)
/// are materialised inside a per-spawn [`TempDir`] returned in
/// `ManagedSpawn.wrapper_dir`. The shell sources them at startup;
/// the directory survives until the caller drops the `ManagedSpawn`
/// (or whatever holds it — in production, `PaneSession`).
pub fn managed_spawn_for(shell: ShellSpec, session_id: &str) -> io::Result<ManagedSpawn> {
    let opt_out = std::env::var_os("TERMICA_NO_SHELL_INTEGRATION").is_some();
    managed_spawn_for_inner(shell, session_id, opt_out)
}

/// Inner spawn-plan builder with the env-var opt-out factored out
/// to a parameter. Lets unit tests exercise both branches without
/// mutating process-wide env (which would require `unsafe` under
/// Rust 2024 and is forbidden crate-wide).
fn managed_spawn_for_inner(
    shell: ShellSpec,
    session_id: &str,
    opt_out: bool,
) -> io::Result<ManagedSpawn> {
    if opt_out {
        return Ok(unmanaged_spawn(shell, session_id));
    }
    let mut env = common_env(shell, session_id);
    match shell {
        ShellSpec::Zsh => {
            // Materialise a per-spawn wrapper directory containing our
            // `.zshrc` and an empty `.zshenv`. Spawn zsh interactively
            // with `ZDOTDIR=<wrapper>` so it sources OUR `.zshrc`
            // instead of the user's. Our `.zshrc` immediately unsets
            // ZDOTDIR (so user code never sees the mutation) and
            // manually sources `~/.zshenv` and the user's real
            // `$ZDOTDIR/.zshrc` in the order vanilla zsh would have.
            //
            // We chose ZDOTDIR over the "PTY-write hidden bootstrap"
            // mechanism originally drafted in spec/03 because zsh's
            // Zsh Line Editor (ZLE) intercepts PTY-stdin bytes as
            // keystrokes once the shell is interactive, so a
            // multi-line bootstrap written to the PTY ends up
            // character-shuffled through line editing instead of
            // executed as commands.
            let wrapper = new_wrapper_dir("zsh-wrapper-")?;
            write_atomic(&wrapper.path().join(".zshrc"), ZSH_BOOTSTRAP)?;
            // Empty `.zshenv` so zsh's initial `$ZDOTDIR/.zshenv`
            // lookup succeeds without falling through to a missing
            // file — we source the user's `$HOME/.zshenv` ourselves
            // inside `.zshrc`, in the right order, after unsetting
            // ZDOTDIR.
            write_atomic(&wrapper.path().join(".zshenv"), "")?;
            let wrapper_str = wrapper.path().to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 wrapper path")
            })?;
            env.push(("ZDOTDIR".into(), wrapper_str.to_string()));
            Ok(ManagedSpawn {
                argv: vec!["zsh".into(), "-i".into()],
                pty_bootstrap: None,
                env,
                wrapper_dir: Some(wrapper),
            })
        }
        ShellSpec::Bash => {
            let wrapper = new_wrapper_dir("bash-wrapper-")?;
            let wrapper_path = wrapper.path().join("bash-bootstrap.bash");
            write_atomic(&wrapper_path, BASH_BOOTSTRAP)?;
            write_atomic(&wrapper.path().join("bash-preexec.sh"), BASH_PREEXEC)?;
            let wrapper_str = wrapper_path
                .to_str()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 wrapper path"))?
                .to_string();
            // NOTE: we deliberately do NOT pass `--norc`. On bash 3.2
            // (the version Apple ships with macOS), `--norc` overrides
            // `--rcfile` — bash honors --norc, skips our wrapper, and
            // the bootstrap never runs. Without --norc, bash reads
            // our --rcfile instead of `~/.bashrc` (which is what
            // --rcfile is documented to do); the wrapper itself
            // sources the user's real `~/.bashrc` from inside, so the
            // user's config still runs in the right order.
            //
            // `--noediting` disables readline. Per spec/04, Termica's
            // `PromptEditor` (Phase 4B) is the line editor; readline
            // would race with it for the tty (readline puts the tty
            // in raw mode and redraws the command line with escape
            // sequences that defeat our prefix-match echo
            // suppression). Without readline, bash reads canonical-
            // mode lines from the kernel; the kernel's deterministic
            // ECHO is what our `EchoSuppressor` is designed for. The
            // wrapper bootstrap also clears `PS1` so bash never
            // prints a competing prompt.
            Ok(ManagedSpawn {
                argv: vec![
                    "bash".into(),
                    "--noprofile".into(),
                    "--noediting".into(),
                    "--rcfile".into(),
                    wrapper_str,
                    "-i".into(),
                ],
                pty_bootstrap: None,
                env,
                wrapper_dir: Some(wrapper),
            })
        }
        ShellSpec::Fish => Ok(ManagedSpawn {
            argv: vec![
                "fish".into(),
                "--no-config".into(),
                "--init-command".into(),
                FISH_BOOTSTRAP.to_string(),
                "-i".into(),
            ],
            pty_bootstrap: None,
            env,
            wrapper_dir: None,
        }),
    }
}

/// Create a per-spawn temp directory under `$TMPDIR` (macOS) /
/// `/tmp` (Linux). The prefix names the shell so debugging is
/// easier; tempfile appends a random suffix.
fn new_wrapper_dir(prefix: &str) -> io::Result<TempDir> {
    tempfile::Builder::new().prefix(prefix).tempdir()
}

/// Fallback spawn when `TERMICA_NO_SHELL_INTEGRATION=1` is set.
/// Launches the user's shell with normal rc processing; no bootstrap.
/// The pane will stay in `Bootstrapping` until the timeout, then
/// transition to `Degraded`.
fn unmanaged_spawn(shell: ShellSpec, session_id: &str) -> ManagedSpawn {
    let argv = match shell {
        ShellSpec::Zsh => vec!["zsh".into(), "-i".into()],
        ShellSpec::Bash => vec!["bash".into(), "-i".into()],
        ShellSpec::Fish => vec!["fish".into(), "-i".into()],
    };
    ManagedSpawn {
        argv,
        pty_bootstrap: None,
        env: common_env(shell, session_id),
        wrapper_dir: None,
    }
}

fn common_env(shell: ShellSpec, session_id: &str) -> Vec<(String, String)> {
    vec![
        ("TERM_PROGRAM".into(), "Termica".into()),
        ("TERMICA_SESSION_ID".into(), session_id.into()),
        ("TERMICA_INTEGRATION_VERSION".into(), INTEGRATION_VERSION.to_string()),
        ("TERMICA_SHELL_INTEGRATION".into(), "1".into()),
        ("TERMICA_SHELL".into(), shell.name().into()),
    ]
}

/// Atomically write `content` to `path`. Writes to a sibling temp
/// file then renames, so a crash mid-write never leaves a torn file
/// in place.
fn write_atomic(path: &Path, content: &str) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path must have a parent directory")
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("termica-tmp")
    ));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

/// Generate a fresh session ID. UUIDv4 in canonical hyphenated form.
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_spec_from_shell_path_handles_bash() {
        assert_eq!(ShellSpec::from_shell_path("/bin/bash"), ShellSpec::Bash);
        assert_eq!(ShellSpec::from_shell_path("/usr/local/bin/bash"), ShellSpec::Bash);
    }

    #[test]
    fn shell_spec_from_shell_path_handles_zsh() {
        assert_eq!(ShellSpec::from_shell_path("/bin/zsh"), ShellSpec::Zsh);
        assert_eq!(ShellSpec::from_shell_path("zsh"), ShellSpec::Zsh);
    }

    #[test]
    fn shell_spec_from_shell_path_handles_fish() {
        assert_eq!(ShellSpec::from_shell_path("/opt/homebrew/bin/fish"), ShellSpec::Fish);
    }

    #[test]
    fn shell_spec_from_shell_path_defaults_to_zsh_for_unknown() {
        assert_eq!(ShellSpec::from_shell_path("/bin/ksh"), ShellSpec::Zsh);
        assert_eq!(ShellSpec::from_shell_path(""), ShellSpec::Zsh);
    }

    #[test]
    fn managed_spawn_zsh_uses_zdotdir_wrapper() {
        let spec = managed_spawn_for_inner(ShellSpec::Zsh, "session-id", false).expect("spawn");
        assert_eq!(spec.argv, vec!["zsh".to_string(), "-i".into()]);
        // ZDOTDIR redirect; no PTY-write needed.
        assert!(spec.pty_bootstrap.is_none());
        let env_keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(env_keys.contains(&"TERMICA_SESSION_ID"));
        assert!(env_keys.contains(&"TERM_PROGRAM"));
        assert!(env_keys.contains(&"ZDOTDIR"));
        // The wrapper file should have been materialised on disk.
        let zdotdir = spec
            .env
            .iter()
            .find(|(k, _)| k == "ZDOTDIR")
            .map(|(_, v)| v.clone())
            .expect("ZDOTDIR set");
        let zshrc = std::path::PathBuf::from(&zdotdir).join(".zshrc");
        assert!(zshrc.exists(), ".zshrc should exist under {zdotdir}");
        let body = std::fs::read_to_string(&zshrc).expect("read .zshrc");
        assert!(body.contains("termica_emit_raw"), ".zshrc should be Termica's bootstrap");
    }

    #[test]
    fn managed_spawn_fish_carries_init_command() {
        let spec = managed_spawn_for_inner(ShellSpec::Fish, "x", false).expect("spawn");
        assert_eq!(spec.argv[0], "fish");
        assert!(spec.argv.iter().any(|a| a == "--no-config"));
        assert!(spec.argv.iter().any(|a| a == "--init-command"));
        assert!(spec.pty_bootstrap.is_none());
        // The bootstrap is shipped inline as one argv slot.
        assert!(spec.argv.iter().any(|a| a.contains("termica_emit_raw")));
    }

    #[test]
    fn opt_out_uses_unmanaged_spawn() {
        let spec = managed_spawn_for_inner(ShellSpec::Zsh, "x", true).expect("spawn");
        assert_eq!(spec.argv, vec!["zsh", "-i"]);
        assert!(spec.pty_bootstrap.is_none());
    }

    #[test]
    fn unmanaged_spawn_still_sets_termica_env() {
        // Even when integration is opted out we want children to know
        // they're running inside Termica (status header, status indicators).
        let spec = managed_spawn_for_inner(ShellSpec::Bash, "x", true).expect("spawn");
        let env_keys: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(env_keys.contains(&"TERM_PROGRAM"));
        assert!(env_keys.contains(&"TERMICA_SESSION_ID"));
    }

    #[test]
    fn new_session_id_returns_uuid_shape() {
        let id = new_session_id();
        // 8-4-4-4-12 hex with hyphens = 36 chars.
        assert_eq!(id.len(), 36);
        assert_eq!(id.chars().filter(|&c| c == '-').count(), 4);
    }
}
