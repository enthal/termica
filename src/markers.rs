//! Shell-integration lifecycle messages.
//!
//! Per [spec/03](../spec/03-shell-integration.md), Termica's
//! integration scripts emit lifecycle messages via DCS-JSON:
//!
//! ```text
//! ESC P Termica;{"type":"...","session":"...","value":...} ESC \
//! ```
//!
//! This module is **only the parser**. The byte-stream → DCS-body
//! buffering and dispatch lives in [`crate::osc`] (a `vte::Perform`
//! impl that buffers DCS bytes between `hook` and `unhook` and hands
//! us the completed body via [`parse_dcs_body`]).
//!
//! Strict rule from spec/03: no code anywhere in Termica scans the
//! raw byte stream for marker patterns. The DCS framing is handled
//! by `vte::Parser`; the JSON payload is parsed here via `serde_json`.
//!
//! Termica's parser ignores OSC 133 and OSC 1337 entirely. Foreign
//! integration scripts (iTerm2 / WezTerm / kitty / Ghostty) may emit
//! those into the byte stream from a user's `.zshrc`; Termica does
//! not trust them. See spec/05 safety rule 3.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// One lifecycle message emitted by Termica's integration script.
/// The `PromptController` ([spec/05](../spec/05-pane-modes.md))
/// consumes these in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleEvent {
    /// `{"type":"integration_ready","value":{"shell":"zsh","version":1}}`
    /// — the bootstrap completed successfully. Termica transitions
    /// the pane from `Bootstrapping` → `RawTerminal` on receipt.
    IntegrationReady { shell: ShellKind, version: u32 },
    /// `{"type":"integration_error","value":"<reason>"}` — the
    /// bootstrap detected a problem and chose to fail loud rather
    /// than continue. Pane transitions to `Degraded`.
    IntegrationError { reason: String },
    /// `{"type":"preexec","value":"<command>"}` — about to execute
    /// the command. Emitted by the shell-side preexec hook.
    Preexec { command: String },
    /// `{"type":"command_finished","value":<exit_int>}` — the
    /// foreground command returned with this exit code.
    CommandFinished { exit: i32 },
    /// `{"type":"precmd","value":"<cwd>"}` — the shell is about to
    /// draw the next prompt. The signal that promotes to
    /// `ShellPromptEditor`.
    Precmd { cwd: PathBuf },
    /// `{"type":"cwd","value":"<cwd>"}` — optional standalone cwd
    /// update outside the precmd flow.
    Cwd { cwd: PathBuf },
    /// `{"type":"prompt_vars","value":{...}}` — open-ended structured
    /// prompt metadata (git branch, virtualenv, etc.) for the native
    /// status header.
    PromptVars { vars: serde_json::Map<String, serde_json::Value> },
    /// `{"type":"command_aborted","value":"<reason>"}` — user cancelled
    /// input before execution (Ctrl-C on empty editor, syntax error
    /// rejected by shell, etc.).
    CommandAborted { reason: String },
}

/// The shell kinds recognised in the `integration_ready` payload.
/// `Unknown` is reserved for values the parser doesn't know about
/// yet, so callers can pattern-match exhaustively without losing
/// information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    Unknown,
}

/// Parse a complete DCS body (the bytes between `ESC P` and `ESC \`)
/// into a [`LifecycleEvent`] if it's one of ours.
///
/// Termica's framing requires the body to start with the literal
/// ASCII `Termica;` followed by a single JSON object. Anything else
/// (other tools' DCS sequences, malformed payloads, JSON we don't
/// recognise) returns `None`.
pub fn parse_dcs_body(body: &[u8]) -> Option<LifecycleEvent> {
    let body = body.strip_prefix(b"Termica;")?;
    let s = std::str::from_utf8(body).ok()?;
    let value: serde_json::Value = serde_json::from_str(s).ok()?;
    parse_message(&value)
}

/// Parse a single JSON message object into a [`LifecycleEvent`].
/// The expected shape is `{"type":"...","session":"...","value":...}`.
/// We do not currently validate `session` (the consumer side may
/// later compare against the spawn-time session ID to discard stale
/// messages from a child that lingered after restart).
fn parse_message(value: &serde_json::Value) -> Option<LifecycleEvent> {
    let obj = value.as_object()?;
    let type_str = obj.get("type")?.as_str()?;
    let value = obj.get("value");
    match type_str {
        "integration_ready" => {
            let v = value?.as_object()?;
            let shell = match v.get("shell").and_then(|s| s.as_str())? {
                "bash" => ShellKind::Bash,
                "zsh" => ShellKind::Zsh,
                "fish" => ShellKind::Fish,
                _ => ShellKind::Unknown,
            };
            let version = v.get("version").and_then(|n| n.as_u64())? as u32;
            Some(LifecycleEvent::IntegrationReady { shell, version })
        }
        "integration_error" => {
            let reason = value?.as_str()?.to_string();
            Some(LifecycleEvent::IntegrationError { reason })
        }
        "preexec" => {
            let command = value?.as_str()?.to_string();
            Some(LifecycleEvent::Preexec { command })
        }
        "command_finished" => {
            // Accept both integer and numeric-string forms.
            let exit = match value? {
                serde_json::Value::Number(n) => n.as_i64()? as i32,
                serde_json::Value::String(s) => s.parse::<i32>().ok()?,
                _ => return None,
            };
            Some(LifecycleEvent::CommandFinished { exit })
        }
        "precmd" => {
            let cwd = PathBuf::from(value?.as_str()?);
            Some(LifecycleEvent::Precmd { cwd })
        }
        "cwd" => {
            let cwd = PathBuf::from(value?.as_str()?);
            Some(LifecycleEvent::Cwd { cwd })
        }
        "prompt_vars" => {
            let vars = value?.as_object()?.clone();
            Some(LifecycleEvent::PromptVars { vars })
        }
        "command_aborted" => {
            let reason = value?.as_str()?.to_string();
            Some(LifecycleEvent::CommandAborted { reason })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the DCS-JSON parser. The byte-stream → DCS-body
    //! buffering is tested via `crate::osc` integration tests; here
    //! we test the parser in isolation by feeding it pre-assembled
    //! bodies.

    use super::*;

    fn body(json: &str) -> Vec<u8> {
        let mut b = b"Termica;".to_vec();
        b.extend_from_slice(json.as_bytes());
        b
    }

    #[test]
    fn integration_ready_zsh_v1() {
        let b = body(
            r#"{"type":"integration_ready","session":"x","value":{"shell":"zsh","version":1}}"#,
        );
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::IntegrationReady { shell: ShellKind::Zsh, version: 1 })
        );
    }

    #[test]
    fn integration_ready_bash_v1() {
        let b = body(
            r#"{"type":"integration_ready","session":"x","value":{"shell":"bash","version":1}}"#,
        );
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::IntegrationReady { shell: ShellKind::Bash, version: 1 })
        );
    }

    #[test]
    fn integration_ready_fish_v1() {
        let b = body(
            r#"{"type":"integration_ready","session":"x","value":{"shell":"fish","version":1}}"#,
        );
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::IntegrationReady { shell: ShellKind::Fish, version: 1 })
        );
    }

    #[test]
    fn integration_ready_unknown_shell_carries_through() {
        let b = body(
            r#"{"type":"integration_ready","session":"x","value":{"shell":"ksh","version":1}}"#,
        );
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::IntegrationReady { shell: ShellKind::Unknown, version: 1 })
        );
    }

    #[test]
    fn integration_error_carries_reason() {
        let b =
            body(r#"{"type":"integration_error","session":"x","value":"bash-preexec.sh missing"}"#);
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::IntegrationError { reason: "bash-preexec.sh missing".into() })
        );
    }

    #[test]
    fn preexec_command_string() {
        let b = body(r#"{"type":"preexec","session":"x","value":"ls -la"}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::Preexec { command: "ls -la".into() }));
    }

    #[test]
    fn command_finished_zero_exit() {
        let b = body(r#"{"type":"command_finished","session":"x","value":0}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::CommandFinished { exit: 0 }));
    }

    #[test]
    fn command_finished_nonzero_exit() {
        let b = body(r#"{"type":"command_finished","session":"x","value":127}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::CommandFinished { exit: 127 }));
    }

    #[test]
    fn command_finished_accepts_string_form() {
        // Some shells emit `$?` as a string because of how printf
        // sprintf-formats arguments. Be liberal in what we accept.
        let b = body(r#"{"type":"command_finished","session":"x","value":"42"}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::CommandFinished { exit: 42 }));
    }

    #[test]
    fn precmd_carries_cwd() {
        let b = body(r#"{"type":"precmd","session":"x","value":"/Users/tim/code"}"#);
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::Precmd { cwd: PathBuf::from("/Users/tim/code") })
        );
    }

    #[test]
    fn cwd_carries_cwd() {
        let b = body(r#"{"type":"cwd","session":"x","value":"/tmp"}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::Cwd { cwd: PathBuf::from("/tmp") }));
    }

    #[test]
    fn prompt_vars_carries_object() {
        let b = body(
            r#"{"type":"prompt_vars","session":"x","value":{"git_branch":"main","git_dirty":false}}"#,
        );
        let parsed = parse_dcs_body(&b).expect("should parse");
        match parsed {
            LifecycleEvent::PromptVars { vars } => {
                assert_eq!(vars.get("git_branch").and_then(|v| v.as_str()), Some("main"));
                assert_eq!(vars.get("git_dirty").and_then(|v| v.as_bool()), Some(false));
            }
            other => panic!("expected PromptVars, got {other:?}"),
        }
    }

    #[test]
    fn command_aborted_carries_reason() {
        let b = body(r#"{"type":"command_aborted","session":"x","value":"syntax error"}"#);
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::CommandAborted { reason: "syntax error".into() })
        );
    }

    // ---- rejected payloads ------------------------------------------

    #[test]
    fn missing_termica_prefix_returns_none() {
        // Foreign DCS sequence (e.g. Sixel, Kitty graphics) — body
        // doesn't start with `Termica;`, so we ignore it.
        let b = b"q...some-other-dcs-body".to_vec();
        assert_eq!(parse_dcs_body(&b), None);
    }

    #[test]
    fn malformed_json_returns_none() {
        let b = body(r#"{"type":"integration_ready","value":{not json}"#);
        assert_eq!(parse_dcs_body(&b), None);
    }

    #[test]
    fn unknown_type_returns_none() {
        let b = body(r#"{"type":"yolo","session":"x","value":42}"#);
        assert_eq!(parse_dcs_body(&b), None);
    }

    #[test]
    fn missing_type_field_returns_none() {
        let b = body(r#"{"session":"x","value":42}"#);
        assert_eq!(parse_dcs_body(&b), None);
    }

    #[test]
    fn empty_body_returns_none() {
        assert_eq!(parse_dcs_body(b""), None);
    }

    #[test]
    fn termica_prefix_without_json_returns_none() {
        let b = b"Termica;".to_vec();
        assert_eq!(parse_dcs_body(&b), None);
    }
}
