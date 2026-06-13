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
    /// `{"type":"shell_vars","value":["NAME",...]}` — the names of the
    /// shell's currently-defined variables (parameters), emitted from the
    /// precmd hook and change-gated shell-side. Feeds `$VAR` tab
    /// completion so it reflects the LIVE shell — including non-exported
    /// parameters (`HISTFILE`, `PS1`, …) and runtime `export`s — which the
    /// spawn-time environment snapshot can't see. Names only, never
    /// values (values routinely hold secrets). See [spec/03 §shell_vars].
    ShellVars { names: Vec<String> },
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
    /// `{"type":"continuation","value":""}` — emitted from the
    /// shell's continuation prompt (zsh `PS2`, bash `PS2`) when the
    /// parser sees an incomplete command (e.g. `echo 1 &&` —
    /// trailing `&&` requires a right-hand side). The integration
    /// script's `PS2` substitutes for the user-visible continuation
    /// prompt with a Termica DCS marker; we use this to re-promote
    /// the pane back to `ShellPromptEditor` so the user can keep
    /// editing the multi-line command instead of typing into raw
    /// mode. Spec/04 §"Submission semantics" subset.
    Continuation,
    /// `{"type":"completion","id":N,"value":["<raw complete -C line>", …]}`
    /// — the live pane shell's answer to a completion request Termica wrote
    /// down the PTY (the `complete\t<id>\t<b64-line>` frame; see
    /// [spec/03 §completion](../spec/03-shell-integration.md)). `id`
    /// correlates the reply with its request. `lines` are the **raw**
    /// `complete -C` output lines (each is fish's `value\tdescription`, or a
    /// space-padded multi-column row from a tool like kubectl); they're
    /// parsed into candidates by the one shared fish parser
    /// ([`crate::completion::drivers::parse::parse_fish_complete`]) so the
    /// live path and the one-shot subprocess path can't diverge. The
    /// candidates come from the shell's OWN `complete -C`, so they reflect
    /// runtime state (aliases / functions defined in-session) the one-shot
    /// subprocess can't see. **Inert to the mode machine** — a completion
    /// reply says nothing about whether we're at a prompt; the bootstrap
    /// emits it and loops straight back to `read` without a preexec/precmd,
    /// so the pane mode never moves (spec/05).
    Completion { id: u64, lines: Vec<String> },
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
        "shell_vars" => {
            // Value is an array of variable-name strings. Non-string
            // elements are skipped defensively rather than failing the
            // whole message.
            let names =
                value?.as_array()?.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
            Some(LifecycleEvent::ShellVars { names })
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
        "continuation" => {
            // Value is intentionally empty — the message itself is
            // the signal that the shell's parser is waiting for
            // more input. No payload needed.
            Some(LifecycleEvent::Continuation)
        }
        "completion" => {
            // `id` is a top-level sibling of `value` (it correlates the
            // reply with its request) — the first message to carry a field
            // beyond type/session/value. Required: a reply we can't
            // correlate is useless, so a missing/non-numeric id is dropped.
            let id = obj.get("id")?.as_u64()?;
            // `value` is an array of raw `complete -C` output lines. A
            // non-string element is skipped defensively rather than failing
            // the whole reply — mirrors `shell_vars`.
            let lines =
                value?.as_array()?.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
            Some(LifecycleEvent::Completion { id, lines })
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
    fn shell_vars_array_of_names() {
        let b = body(r#"{"type":"shell_vars","session":"x","value":["HOME","HISTFILE","PATH"]}"#);
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::ShellVars {
                names: vec!["HOME".into(), "HISTFILE".into(), "PATH".into()]
            })
        );
    }

    #[test]
    fn shell_vars_empty_array() {
        let b = body(r#"{"type":"shell_vars","session":"x","value":[]}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::ShellVars { names: vec![] }));
    }

    #[test]
    fn shell_vars_skips_non_string_elements() {
        // Defensive: a malformed element doesn't poison the whole list.
        let b = body(r#"{"type":"shell_vars","session":"x","value":["A",3,"B"]}"#);
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::ShellVars { names: vec!["A".into(), "B".into()] })
        );
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

    // ---- completion (live-shell completion reply) -------------------

    #[test]
    fn completion_marker_parses_id_and_raw_lines() {
        // `value` is the raw `complete -C` lines (tab-separated, or a
        // space-padded kubectl row); parsing into candidates happens later
        // in the shared fish parser, not here.
        let b = body(
            r#"{"type":"completion","session":"x","id":7,"value":["hello\talias hello=echo HI","help","deployments    deploy    apps/v1"]}"#,
        );
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::Completion {
                id: 7,
                lines: vec![
                    "hello\talias hello=echo HI".into(),
                    "help".into(),
                    "deployments    deploy    apps/v1".into(),
                ],
            })
        );
    }

    #[test]
    fn completion_marker_empty_value_is_empty_list() {
        // `complete -C` found nothing — the reply still fires (so the
        // request never hangs), just with no lines.
        let b = body(r#"{"type":"completion","session":"x","id":1,"value":[]}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::Completion { id: 1, lines: vec![] }));
    }

    #[test]
    fn completion_marker_missing_id_returns_none() {
        // No id → we couldn't correlate it to a request → drop it.
        let b = body(r#"{"type":"completion","session":"x","value":[]}"#);
        assert_eq!(parse_dcs_body(&b), None);
    }

    #[test]
    fn completion_marker_skips_non_string_line() {
        // A non-string element is skipped defensively rather than failing
        // the whole reply.
        let b = body(r#"{"type":"completion","session":"x","id":2,"value":["ok",42,"two"]}"#);
        assert_eq!(
            parse_dcs_body(&b),
            Some(LifecycleEvent::Completion { id: 2, lines: vec!["ok".into(), "two".into()] })
        );
    }

    #[test]
    fn continuation_event_parses() {
        let b = body(r#"{"type":"continuation","session":"x","value":""}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::Continuation));
    }

    #[test]
    fn continuation_event_parses_without_value() {
        // The schema says `value` is optional for continuation —
        // accept the form the shell actually emits.
        let b = body(r#"{"type":"continuation","session":"x"}"#);
        assert_eq!(parse_dcs_body(&b), Some(LifecycleEvent::Continuation));
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
