//! Pure parsing, request construction, and routing predicates for the
//! CLI-native completion drivers ([spec/04a §"Source 1"](../../../spec/04a-completion.md)).
//!
//! Everything here is pure: it turns recorded subprocess stdout and the
//! editor line into [`CompletionCandidate`]s, or decides which driver
//! (if any) should fire for an editor state. No spawning — that lives in
//! the parent [`super`] engine behind the [`super::DriverRunner`] seam —
//! so all of this is unit-tested over fixtures with no processes
//! involved.

use super::DriverTool;
use crate::completion::local;
use crate::completion::{CompletionCandidate, CompletionSource};

/// Map a command name to the driver that handles it, or `None` when no
/// CLI-native driver is known. Pass the command's **basename**
/// (`kubectl`, not `/usr/local/bin/kubectl`).
pub fn driver_tool_for_command(command: &str) -> Option<DriverTool> {
    Some(match command {
        "kubectl" => DriverTool::Kubectl,
        "gh" => DriverTool::Gh,
        "docker" => DriverTool::Docker,
        "aws" => DriverTool::Aws,
        "git" => DriverTool::Git,
        _ => return None,
    })
}

/// Decide whether a CLI-native driver should fire for the editor state,
/// and which. Fires only when the cursor is in **argument** position
/// (not completing the command name itself) and the current command
/// segment's leading word maps to a known driver.
///
/// Returns `(tool, line, point)` where `line` is the current command
/// segment up to the cursor (everything after the last `|`/`;`/`&`,
/// leading whitespace trimmed) and `point` is the cursor's byte offset
/// within `line` — always `line.len()`, since we complete at the cursor
/// by truncating there.
pub fn driver_target(editor_text: &str, cursor: usize) -> Option<(DriverTool, String, usize)> {
    if local::is_command_position(editor_text, cursor) {
        return None;
    }
    let segment = current_command_segment(editor_text, cursor);
    let command = segment.split_whitespace().next()?;
    let base = command.rsplit('/').next().unwrap_or(command);
    let tool = driver_tool_for_command(base)?;
    Some((tool, segment.to_string(), segment.len()))
}

/// The current command segment ending at `cursor`: the text after the
/// last command sequencer (`|`, `;`, `&`) up to the cursor, with leading
/// whitespace trimmed. Quote-aware splitting is a later refinement; this
/// matches the v1 sequencer scan in [`local::is_command_position`].
fn current_command_segment(text: &str, cursor: usize) -> &str {
    let cursor = crate::prompt_editor::clamp_to_char_boundary(text, cursor);
    let prefix = text.get(..cursor).unwrap_or("");
    let seg_start = prefix.rfind(['|', ';', '&']).map(|i| i + 1).unwrap_or(0);
    prefix.get(seg_start..).unwrap_or("").trim_start()
}

/// The token currently being completed at the end of `line`: the last
/// whitespace-delimited word, or `""` when `line` ends in whitespace
/// (the user is starting a fresh argument). Used to prefix-filter
/// drivers that return an unfiltered list (`git --list-cmds`).
pub fn last_word(line: &str) -> &str {
    if line.ends_with(|c: char| c.is_whitespace()) {
        return "";
    }
    line.split_whitespace().next_back().unwrap_or("")
}

/// Build the argv for `<tool> __complete …` from the current command
/// segment. The leading binary word is dropped (cobra wants the args
/// that follow it); a trailing empty arg is appended when `line` ends in
/// whitespace so cobra completes the **next** (fresh) argument rather
/// than re-completing the last word.
///
/// - `"kubectl get po"` → `["__complete", "get", "po"]`
/// - `"kubectl get "`   → `["__complete", "get", ""]`
/// - `"kubectl "`       → `["__complete", ""]`
pub fn cobra_complete_args(line: &str) -> Vec<String> {
    let mut words = line.split_whitespace();
    let _binary = words.next();
    let mut args = vec!["__complete".to_string()];
    args.extend(words.map(str::to_string));
    if line.ends_with(|c: char| c.is_whitespace()) {
        args.push(String::new());
    }
    args
}

/// The two environment variables `aws_completer` reads: the full command
/// line and the cursor's byte offset within it.
pub fn aws_completer_env(line: &str, point: usize) -> [(String, String); 2] {
    [("COMP_LINE".to_string(), line.to_string()), ("COMP_POINT".to_string(), point.to_string())]
}

/// Parse a driver's raw stdout into candidates, dispatching on the tool.
/// `line` is the command segment (used to derive git's filter prefix).
pub fn parse_for(tool: DriverTool, stdout: &str, line: &str) -> Vec<CompletionCandidate> {
    match tool {
        DriverTool::Kubectl | DriverTool::Gh | DriverTool::Docker => {
            parse_cobra_complete(stdout, tool)
        }
        DriverTool::Aws => parse_aws_completer(stdout),
        DriverTool::Git => parse_git_list_cmds(stdout, last_word(line)),
    }
}

/// Parse cobra's `__complete` stdout: one `value\tdescription` per line
/// (description optional), terminated by a `:N` directive line we ignore.
/// kubectl's *resource* completion deviates from the tab convention and
/// pads columns with spaces instead, so a run of two or more spaces is
/// also accepted as the value/description boundary (cobra completion
/// values never contain spaces).
pub fn parse_cobra_complete(stdout: &str, tool: DriverTool) -> Vec<CompletionCandidate> {
    let source = CompletionSource::Driver(tool);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.starts_with(':'))
        .map(|line| match split_value_desc(line) {
            (value, Some(desc)) => CompletionCandidate::with_description(value, desc, source),
            (value, None) => CompletionCandidate::simple(value, source),
        })
        .collect()
}

/// Split a cobra completion line into `(value, description)`. The
/// boundary is the first tab, or failing that the first run of two or
/// more spaces (kubectl's column padding); a line with neither is all
/// value. The value is trimmed of trailing spaces; an empty description
/// becomes `None`.
fn split_value_desc(line: &str) -> (&str, Option<&str>) {
    // `(value_end, desc_start)`: for a tab, skip the tab byte; for a
    // space run, the trailing `.trim()` on the description eats the rest.
    let boundary = line.find('\t').map(|i| (i, i + 1)).or_else(|| line.find("  ").map(|i| (i, i)));
    match boundary {
        Some((value_end, desc_start)) => {
            let value = line.get(..value_end).unwrap_or(line).trim_end();
            let desc = line.get(desc_start..).unwrap_or("").trim();
            (value, (!desc.is_empty()).then_some(desc))
        }
        None => (line.trim_end(), None),
    }
}

/// Parse `aws_completer` stdout: whitespace-separated candidates, no
/// descriptions.
pub fn parse_aws_completer(stdout: &str) -> Vec<CompletionCandidate> {
    stdout
        .split_whitespace()
        .map(|tok| CompletionCandidate::simple(tok, CompletionSource::Driver(DriverTool::Aws)))
        .collect()
}

/// Parse `git --list-cmds=…` stdout: one subcommand name per line. git's
/// endpoint returns the full list unfiltered, so we prefix-filter by the
/// partial word the user is completing.
pub fn parse_git_list_cmds(stdout: &str, partial: &str) -> Vec<CompletionCandidate> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| l.starts_with(partial))
        .map(|name| CompletionCandidate::simple(name, CompletionSource::Driver(DriverTool::Git)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KUBECTL_SUBCMDS: &str =
        include_str!("../../../testdata/completion/kubectl/subcommands.txt");
    const KUBECTL_RESOURCES: &str =
        include_str!("../../../testdata/completion/kubectl/get-resources.txt");
    const GH_PR: &str = include_str!("../../../testdata/completion/gh/pr.txt");
    const DOCKER_RUN: &str = include_str!("../../../testdata/completion/docker/run-images.txt");
    const AWS_S3: &str = include_str!("../../../testdata/completion/aws/s3.txt");
    const GIT_LIST: &str = include_str!("../../../testdata/completion/git/list-cmds.txt");

    fn as_strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    // ---- cobra arg construction (the must-fail-first surface) --------

    #[test]
    fn cobra_args_appends_empty_trailing_arg_on_trailing_space() {
        assert_eq!(as_strs(&cobra_complete_args("kubectl get ")), ["__complete", "get", ""]);
    }

    #[test]
    fn cobra_args_drops_binary_and_keeps_words() {
        assert_eq!(as_strs(&cobra_complete_args("kubectl get po")), ["__complete", "get", "po"]);
    }

    #[test]
    fn cobra_args_bare_command_with_trailing_space() {
        assert_eq!(as_strs(&cobra_complete_args("kubectl ")), ["__complete", ""]);
    }

    // ---- cobra parsing over recorded fixtures -----------------------

    #[test]
    fn cobra_parse_gh_splits_tab_descriptions_and_drops_directive() {
        let cands = parse_cobra_complete(GH_PR, DriverTool::Gh);
        let checkout = cands.iter().find(|c| c.value == "checkout").expect("checkout present");
        assert_eq!(checkout.description.as_deref(), Some("Check out a pull request in git"));
        assert_eq!(checkout.source, CompletionSource::Driver(DriverTool::Gh));
        assert!(cands.iter().all(|c| !c.value.starts_with(':')), "directive line dropped");
    }

    #[test]
    fn cobra_parse_kubectl_subcommands_have_descriptions() {
        let cands = parse_cobra_complete(KUBECTL_SUBCMDS, DriverTool::Kubectl);
        let get = cands.iter().find(|c| c.value == "get").expect("get present");
        assert!(get.description.is_some(), "kubectl subcommands carry descriptions");
        assert_eq!(get.source, CompletionSource::Driver(DriverTool::Kubectl));
    }

    #[test]
    fn cobra_parse_kubectl_resources_space_padded_columns() {
        // kubectl get <resource> pads with spaces, not tabs: the value
        // must be JUST the resource name, not the whole column row.
        let cands = parse_cobra_complete(KUBECTL_RESOURCES, DriverTool::Kubectl);
        let pods = cands.iter().find(|c| c.value == "pods").expect("pods present");
        assert_eq!(pods.value, "pods");
        assert_eq!(pods.source, CompletionSource::Driver(DriverTool::Kubectl));
    }

    #[test]
    fn cobra_parse_docker_bare_values_no_description() {
        let cands = parse_cobra_complete(DOCKER_RUN, DriverTool::Docker);
        assert!(!cands.is_empty());
        assert!(
            cands.iter().any(|c| c.value.contains(':') && c.description.is_none()),
            "image refs have no tab → no description"
        );
        assert!(cands.iter().all(|c| !c.value.starts_with(':')), "directive line dropped");
    }

    // ---- aws + git parsing ------------------------------------------

    #[test]
    fn aws_parse_whitespace_tokens_no_descriptions() {
        let cands = parse_aws_completer(AWS_S3);
        assert!(cands.iter().any(|c| c.value == "ls"));
        assert!(cands.iter().any(|c| c.value == "cp"));
        assert!(cands.iter().all(|c| c.description.is_none()));
        assert!(cands.iter().all(|c| c.source == CompletionSource::Driver(DriverTool::Aws)));
    }

    #[test]
    fn aws_env_sets_comp_line_and_point() {
        let env = aws_completer_env("aws s3 ", 7);
        assert_eq!(env[0], ("COMP_LINE".to_string(), "aws s3 ".to_string()));
        assert_eq!(env[1], ("COMP_POINT".to_string(), "7".to_string()));
    }

    #[test]
    fn aws_env_point_is_byte_offset_with_multibyte() {
        // "aws s3 café " is 13 bytes (é is 2 bytes); the point is a BYTE
        // offset, so the env carries the byte length, not the char count.
        let line = "aws s3 café ";
        let env = aws_completer_env(line, line.len());
        assert_eq!(env[1].1, line.len().to_string());
        assert_eq!(env[1].1, "13");
    }

    #[test]
    fn git_list_cmds_prefix_filters() {
        let all = parse_git_list_cmds(GIT_LIST, "");
        let che = parse_git_list_cmds(GIT_LIST, "che");
        assert!(all.len() > che.len());
        assert!(che.iter().any(|c| c.value == "checkout"));
        assert!(che.iter().all(|c| c.value.starts_with("che")));
        assert!(che.iter().all(|c| c.source == CompletionSource::Driver(DriverTool::Git)));
    }

    #[test]
    fn last_word_handles_trailing_space_and_words() {
        assert_eq!(last_word("git che"), "che");
        assert_eq!(last_word("git "), "");
        assert_eq!(last_word("git checkout ma"), "ma");
    }

    // ---- routing predicates -----------------------------------------

    #[test]
    fn driver_tool_for_command_known_and_unknown() {
        assert_eq!(driver_tool_for_command("kubectl"), Some(DriverTool::Kubectl));
        assert_eq!(driver_tool_for_command("gh"), Some(DriverTool::Gh));
        assert_eq!(driver_tool_for_command("git"), Some(DriverTool::Git));
        assert_eq!(driver_tool_for_command("ls"), None);
    }

    #[test]
    fn driver_target_fires_in_arg_position_for_known_tool() {
        let text = "kubectl get po";
        let (tool, line, point) = driver_target(text, text.len()).expect("driver fires");
        assert_eq!(tool, DriverTool::Kubectl);
        assert_eq!(line, "kubectl get po");
        assert_eq!(point, text.len());
    }

    #[test]
    fn driver_target_none_in_command_position() {
        // Completing the command name itself is not a driver's job.
        assert!(driver_target("kubec", 5).is_none());
    }

    #[test]
    fn driver_target_none_for_unknown_command() {
        assert!(driver_target("ls -l", 5).is_none());
    }

    #[test]
    fn driver_target_after_pipe_uses_segment_only() {
        let text = "echo hi | kubectl get po";
        let (tool, line, _) = driver_target(text, text.len()).expect("driver fires after pipe");
        assert_eq!(tool, DriverTool::Kubectl);
        assert_eq!(line, "kubectl get po", "segment after the pipe, leading space trimmed");
    }

    #[test]
    fn driver_target_basenames_absolute_path_command() {
        let text = "/usr/local/bin/kubectl get po";
        let (tool, _, _) = driver_target(text, text.len()).expect("basename maps to a tool");
        assert_eq!(tool, DriverTool::Kubectl);
    }
}
