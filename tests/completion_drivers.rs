//! Integration tests for CLI-native completion drivers
//! ([spec/04a §"Source 1"](../spec/04a-completion.md)).
//!
//! These exercise the **real** tools' completion endpoints against the
//! pure parsers, so we know the parsing matches what the tools actually
//! emit on this machine — not just the recorded fixtures. Per the
//! silent-failure rule ([spec/04a §"Driver detection failure"]), a
//! missing tool **skips** the test (prints a note) rather than failing
//! it: CI without `kubectl`/`gh`/`docker`/`aws` must stay green. `git`
//! is always present in CI, so its case is the always-runnable baseline.

use std::process::Command;

use termica::completion::CompletionSource;
use termica::completion::drivers::DriverTool;
use termica::completion::drivers::parse::{parse_cobra_complete, parse_git_list_cmds};

/// `true` when `tool --version` runs and exits zero.
fn available(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn git_list_cmds_real_output_parses_and_filters() {
    if !available("git") {
        eprintln!("skip: git not installed");
        return;
    }
    let out = Command::new("git")
        .args(["--list-cmds=builtins,others,nohelpers"])
        .output()
        .expect("git --list-cmds runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cands = parse_git_list_cmds(&stdout, "che");
    assert!(
        cands.iter().any(|c| c.value == "checkout"),
        "real git lists `checkout` under the `che` prefix"
    );
    assert!(cands.iter().all(|c| c.value.starts_with("che")));
    assert!(cands.iter().all(|c| c.source == CompletionSource::Driver(DriverTool::Git)));
}

#[test]
fn gh_complete_real_output_parses() {
    if !available("gh") {
        eprintln!("skip: gh not installed");
        return;
    }
    let out =
        Command::new("gh").args(["__complete", "pr", ""]).output().expect("gh __complete runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cands = parse_cobra_complete(&stdout, DriverTool::Gh);
    assert!(!cands.is_empty(), "gh emits pr subcommand candidates");
    // cobra descriptions come through; the `:N` directive line is dropped.
    assert!(cands.iter().all(|c| !c.value.starts_with(':')));
    assert!(cands.iter().any(|c| c.description.is_some()));
}

#[test]
fn kubectl_complete_real_output_parses() {
    if !available("kubectl") {
        eprintln!("skip: kubectl not installed");
        return;
    }
    let out =
        Command::new("kubectl").args(["__complete", ""]).output().expect("kubectl __complete runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let cands = parse_cobra_complete(&stdout, DriverTool::Kubectl);
    assert!(!cands.is_empty(), "kubectl emits subcommand candidates");
    assert!(cands.iter().all(|c| !c.value.starts_with(':')));
    // Values are bare subcommand names, never the directive or a
    // whole padded column row.
    assert!(cands.iter().any(|c| c.value == "get"));
}
