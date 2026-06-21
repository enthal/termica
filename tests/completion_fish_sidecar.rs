//! Integration test for the fish **shell sidecar** completion source
//! ([spec/04a §"Source 2"](../spec/04a-completion.md)).
//!
//! Like the CLI-native driver tests, this runs the **real** shell —
//! `fish -c 'complete -C <line>'` — and feeds its stdout through the pure
//! [`parse_fish_complete`] parser, so we know the parsing matches what
//! fish actually emits on this machine, not just the recorded fixture.
//! Per the silent-failure rule a missing `fish` **skips** (CI without
//! fish stays green), matching the driver and multiline tests.

use std::process::Command;

use termica::completion::CompletionSource;
use termica::completion::drivers::DriverTool;
use termica::completion::drivers::parse::parse_fish_complete;

fn locate_fish() -> Option<String> {
    ["/opt/homebrew/bin/fish", "/usr/local/bin/fish", "/usr/bin/fish"]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
}

/// Run `complete -C` for `line`, exactly as the production `FishComplete`
/// driver invocation does (`-c 'complete -C $argv[1]' <line>`), and parse.
fn fish_complete(fish: &str, line: &str) -> Vec<termica::completion::CompletionCandidate> {
    let out = Command::new(fish)
        .args(["-c", "complete -C $argv[1]", line])
        .output()
        .expect("fish -c complete runs");
    parse_fish_complete(&String::from_utf8_lossy(&out.stdout))
}

#[test]
fn fish_complete_real_output_parses_git_subcommands() {
    let Some(fish) = locate_fish() else {
        eprintln!("skip: fish not installed");
        return;
    };
    // fish autoloads git's bundled completion even with no user config.
    let cands = fish_complete(&fish, "git che");
    assert!(
        cands.iter().any(|c| c.value == "checkout"),
        "real fish completes `checkout` for `git che`; got {:?}",
        cands.iter().map(|c| &c.value).collect::<Vec<_>>()
    );
    // Descriptions come through, tagged as the fish sidecar source.
    assert!(cands.iter().all(|c| c.source == CompletionSource::Driver(DriverTool::FishComplete)));
    assert!(cands.iter().any(|c| c.description.is_some()), "fish emits descriptions");
}

#[test]
fn fish_complete_covers_any_command_not_just_known_drivers() {
    let Some(fish) = locate_fish() else {
        eprintln!("skip: fish not installed");
        return;
    };
    // The whole point of the sidecar: it completes commands that have no
    // per-tool CLI driver. `set -` exercises fish's built-in `set`
    // completion (flags), which is stable and independent of cwd/network.
    let cands = fish_complete(&fish, "set -");
    assert!(
        cands.iter().any(|c| c.value == "-e") && cands.iter().any(|c| c.value == "-a"),
        "fish completes flags for the `set` builtin (no CLI driver exists for it); got {:?}",
        cands.iter().map(|c| &c.value).collect::<Vec<_>>()
    );
    assert!(cands.iter().any(|c| c.description.is_some()), "flag completions carry descriptions");
}
