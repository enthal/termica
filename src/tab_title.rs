//! Tab-title computation + active-pane lookup. Pure tree / string
//! logic, split out so both rules are unit-testable without standing
//! up egui or a real PTY.

use std::path::Path;

use egui_tiles::{Tile, TileId, Tree};

use crate::pane_slot::PaneId;

/// Maximum rendered width of a tab title, in chars. Titles longer
/// than this collapse to `..` + their trailing portion, since the
/// suffix (a leaf dir like `termica`, or a project subdir) is the
/// part the user is actually trying to disambiguate by. Picked by
/// eyeballing: 25 chars fits comfortably without forcing the tab
/// strip to scroll on a default-size window.
const MAX_TAB_TITLE_CHARS: usize = 25;

/// Compute the tab title for a pane.
///
/// Rule, in order:
/// 1. If the shell hasn't yet reported an OSC-7 cwd, fall back to
///    `pane N` so the user has *something* to recognise the tab by.
/// 2. Otherwise, return the cwd as an absolute path, with `$HOME`
///    replaced by `~`:
///    - `cwd == $HOME` → `~`
///    - `cwd` is strictly inside `$HOME` → `~/relative/path`
///    - cwd doesn't sit under `$HOME` (or `$HOME` is unknown) →
///      the cwd as-is.
///
/// Edge cases that bit us in earlier drafts:
/// - `$HOME` may or may not have a trailing slash (`/Users/tim`
///   vs `/Users/tim/`); both should yield the same `~`-substitution.
/// - `cwd` from OSC 7 may or may not have a trailing slash; we
///   normalize both ends so `/Users/tim/` produces `~`, not `~/`.
/// - `/Users/timjones` must NOT match the prefix `/Users/tim` —
///   that would render `~jones` for an unrelated user's dir. The
///   `rest.starts_with('/')` guard prevents that.
///
/// Pure function so the rule is unit-testable without any egui
/// plumbing.
pub fn tab_title_for(pane_id: PaneId, cwd: Option<&Path>, home: Option<&Path>) -> String {
    tab_title_for_with_osc(pane_id, None, cwd, home, None)
}

/// Variant of [`tab_title_for`] that picks the most-informative
/// title given everything the pane knows:
///
/// 1. **Running program** (e.g. `less`, `vim`, `htop`) — when
///    `running_program` is `Some`, take the first whitespace-
///    separated token. This is what surfaces in the tab AND the
///    window title while an alt-screen or long-running program
///    has the foreground; `less ~/big.log` ⇒ tab shows `less`.
/// 2. **Shell-set OSC 0 / 2** title — if non-empty, used as-is.
/// 3. **cwd-derived** — `~/git/enthal/termica` style.
/// 4. **`pane <n>`** — final fallback when none of the above is known.
///
/// The same truncation rule applies to whichever title wins.
pub fn tab_title_for_with_osc(
    pane_id: PaneId,
    osc_title: Option<&str>,
    cwd: Option<&Path>,
    home: Option<&Path>,
    running_program: Option<&str>,
) -> String {
    if let Some(p) = running_program
        && let Some(word) = first_word(p)
    {
        return truncate_tab_title(word);
    }
    if let Some(t) = osc_title
        && !t.trim().is_empty()
    {
        return truncate_tab_title(t);
    }
    let Some(c) = cwd else {
        return format!("pane {}", pane_id.0);
    };
    let raw = home_relative_cwd(c, home);
    truncate_tab_title(&raw)
}

/// First whitespace-separated word of `s`, or `None` when the
/// string is empty/whitespace.
fn first_word(s: &str) -> Option<&str> {
    s.split_whitespace().next()
}

/// Render `cwd` with the user's `$HOME` substituted for `~`, in the
/// same way [`tab_title_for`] does it, but **without** the tab-title
/// truncation. Used by block-header chrome which has more horizontal
/// room than a tab strip and wants to show the full path.
///
/// Rules (same as [`tab_title_for`]):
/// - `cwd == $HOME` → `~`.
/// - `cwd` strictly inside `$HOME` → `~/relative/path`.
/// - Trailing slashes normalized on both ends so `/Users/tim/`
///   resolves to `~`, not `~/`.
/// - `$HOME` unknown, empty, or `cwd` outside `$HOME` → the cwd is
///   returned as-is (sans trailing slash, except for `/` itself).
/// - No tilde-prefix abbreviation for *other* users' home dirs
///   (`~jones`) — only the current `$HOME`.
pub fn home_relative_cwd(cwd: &Path, home: Option<&Path>) -> String {
    let cwd_s = cwd.to_string_lossy();
    let cwd_norm: &str = if cwd_s.as_ref() == "/" { "/" } else { cwd_s.trim_end_matches('/') };

    if let Some(h) = home {
        let home_s = h.to_string_lossy();
        let home_norm = home_s.trim_end_matches('/');
        if !home_norm.is_empty() {
            if cwd_norm == home_norm {
                return "~".to_string();
            }
            if let Some(rest) = cwd_norm.strip_prefix(home_norm)
                && rest.starts_with('/')
            {
                return format!("~{rest}");
            }
        }
    }
    cwd_norm.to_string()
}

/// Truncate a tab title to [`MAX_TAB_TITLE_CHARS`], preserving the
/// *tail* of the string with a `..` prefix when truncation kicks
/// in. Counting is in `char`s, not bytes, so a directory whose name
/// happens to contain multi-byte UTF-8 doesn't get cut mid-codepoint.
fn truncate_tab_title(s: &str) -> String {
    let count = s.chars().count();
    if count <= MAX_TAB_TITLE_CHARS {
        return s.to_string();
    }
    const ELLIPSIS: &str = "..";
    let keep = MAX_TAB_TITLE_CHARS - ELLIPSIS.len();
    let suffix: String = s.chars().skip(count - keep).collect();
    format!("{ELLIPSIS}{suffix}")
}

/// Resolve the active pane id inside a Tabs container, if any. Pure
/// Tree navigation; lifted out of `TermicaApp::spawn_new_pane_in_tabs`
/// so it can be exercised without bringing up a real PTY.
///
/// Returns `None` if `tabs_tile` doesn't refer to a Tabs container,
/// the container has no active child, the active child isn't a leaf
/// pane, or the tile id is dangling.
pub fn active_pane_in_tabs(tree: &Tree<PaneId>, tabs_tile: TileId) -> Option<PaneId> {
    let Tile::Container(egui_tiles::Container::Tabs(tabs)) = tree.tiles.get(tabs_tile)? else {
        return None;
    };
    let active = tabs.active?;
    match tree.tiles.get(active)? {
        Tile::Pane(pane_id) => Some(*pane_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn tab_title_falls_back_to_pane_n_when_cwd_unknown() {
        assert_eq!(tab_title_for(PaneId(3), None, None), "pane 3");
    }

    #[test]
    fn tab_title_with_osc_uses_shell_title_when_present() {
        let cwd = PathBuf::from("/Users/tim/git");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(
            tab_title_for_with_osc(PaneId(0), Some("vim foo.txt"), Some(&cwd), Some(&home), None),
            "vim foo.txt"
        );
    }

    #[test]
    fn tab_title_with_osc_ignores_blank_shell_title() {
        // Empty / whitespace-only titles fall through to the cwd path
        // — a shell that did `\e]2;\a` to reset shouldn't strand the
        // user on a blank tab.
        let cwd = PathBuf::from("/Users/tim");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for_with_osc(PaneId(0), Some(""), Some(&cwd), Some(&home), None), "~");
        assert_eq!(
            tab_title_for_with_osc(PaneId(0), Some("   "), Some(&cwd), Some(&home), None),
            "~"
        );
    }

    #[test]
    fn tab_title_with_osc_falls_back_to_pane_n_when_no_cwd_and_no_title() {
        assert_eq!(tab_title_for_with_osc(PaneId(7), None, None, None, None), "pane 7");
    }

    #[test]
    fn tab_title_running_program_overrides_cwd_and_osc() {
        let cwd = PathBuf::from("/Users/tim/git/enthal/termica");
        let home = PathBuf::from("/Users/tim");
        // While `less ~/big.log` is running, the tab reads `less`.
        assert_eq!(
            tab_title_for_with_osc(
                PaneId(0),
                Some("shell-set-title"),
                Some(&cwd),
                Some(&home),
                Some("less ~/big.log"),
            ),
            "less"
        );
        // Multi-arg programs still surface as the first word.
        assert_eq!(
            tab_title_for_with_osc(
                PaneId(0),
                None,
                Some(&cwd),
                Some(&home),
                Some("vim --noplugin foo.txt"),
            ),
            "vim"
        );
        // Empty / whitespace running program falls through.
        assert_eq!(
            tab_title_for_with_osc(PaneId(0), None, Some(&cwd), Some(&home), Some("")),
            "~/git/enthal/termica"
        );
    }

    #[test]
    fn tab_title_with_osc_truncates_long_shell_titles() {
        // 100-char OSC title — the same truncation rule that applies
        // to cwd-derived titles should apply here.
        let long: String = "x".repeat(100);
        let out = tab_title_for_with_osc(PaneId(0), Some(&long), None, None, None);
        assert!(out.len() < long.len(), "long titles should be truncated; got {out}");
    }

    #[test]
    fn tab_title_uses_full_cwd_when_outside_home() {
        let cwd = PathBuf::from("/tmp");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "/tmp");
    }

    #[test]
    fn tab_title_uses_full_cwd_when_home_unknown() {
        // Short cwd so the truncation rule doesn't kick in and
        // obscure what this test is asserting.
        let cwd = PathBuf::from("/Users/tim/git");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), None), "/Users/tim/git");
    }

    #[test]
    fn tab_title_substitutes_tilde_when_cwd_is_home() {
        let cwd = PathBuf::from("/Users/tim");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~");
    }

    #[test]
    fn tab_title_substitutes_tilde_with_subpath() {
        let cwd = PathBuf::from("/Users/tim/git/enthal/termica");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~/git/enthal/termica");
    }

    #[test]
    fn tab_title_tolerates_trailing_slash_on_home() {
        let cwd = PathBuf::from("/Users/tim/git/enthal/termica");
        let home = PathBuf::from("/Users/tim/");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~/git/enthal/termica");
    }

    #[test]
    fn tab_title_tolerates_trailing_slash_on_cwd_at_home() {
        let cwd = PathBuf::from("/Users/tim/");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "~");
    }

    #[test]
    fn tab_title_does_not_match_sibling_user_dir_as_home() {
        // The bug we're guarding against: HOME=/Users/tim must NOT
        // match `/Users/timjones` as a "tim/jones" subdir and yield
        // `~jones`. The next char after the prefix must be a `/`.
        let cwd = PathBuf::from("/Users/timjones/work");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(0), Some(&cwd), Some(&home)), "/Users/timjones/work");
    }

    #[test]
    fn tab_title_returns_root_as_root() {
        // `/` as the cwd renders as `/`, not as a fallback.
        let cwd = PathBuf::from("/");
        let home = PathBuf::from("/Users/tim");
        assert_eq!(tab_title_for(PaneId(7), Some(&cwd), Some(&home)), "/");
    }

    #[test]
    fn tab_title_unchanged_when_at_max_length() {
        // Exactly 25 chars: `~/` (2) + 23-char dir name.
        let cwd = PathBuf::from("/Users/tim/abcdefghijklmnopqrstuvw");
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert_eq!(title, "~/abcdefghijklmnopqrstuvw");
    }

    #[test]
    fn tab_title_truncates_long_titles_with_leading_ellipsis() {
        // 42-char `~`-substituted title → expect 25 chars with `..`
        // prefix and the last 23 chars retained.
        let cwd = PathBuf::from("/Users/tim/very/long/path/with/lots/of/subdirs/here");
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert!(title.starts_with(".."));
        // Tail preserved — the user cares about the deepest dir.
        assert!(title.ends_with("/here"));
    }

    #[test]
    fn tab_title_truncates_unsubstituted_paths_too() {
        // Outside `$HOME`, same rule applies.
        let cwd = PathBuf::from("/var/log/very/long/path/under/system/dirs/please");
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        assert!(title.starts_with(".."));
    }

    #[test]
    fn tab_title_truncation_is_char_safe_for_multibyte() {
        // A name longer than 25 with multi-byte chars must not be
        // sliced mid-codepoint. Using accented chars: each `é` is
        // 2 bytes but 1 char; `truncate_tab_title` counts in chars.
        let s = "/Users/tim/répertoire-très-long-avec-accents-partout/";
        let cwd = PathBuf::from(s);
        let home = PathBuf::from("/Users/tim");
        let title = tab_title_for(PaneId(0), Some(&cwd), Some(&home));
        assert_eq!(title.chars().count(), MAX_TAB_TITLE_CHARS);
        // If we'd sliced bytes, this would have panicked above.
    }

    // --- active pane resolution (Phase 2B spawn-in-cwd) -------------
    //
    // These exercise the pure Tree navigation under
    // [`active_pane_in_tabs`]. The "spawn in cwd" wiring on top of
    // it (turn a pane id into its OSC 7 cwd, hand to PtyConfig) is
    // covered by the OSC 7 tracking tests and end-to-end UX use.

    #[test]
    fn active_pane_in_tabs_returns_default_active_for_single_child() {
        let mut tree = Tree::<PaneId>::empty("test-single");
        let pane_tile = tree.tiles.insert_pane(PaneId(1));
        let tabs_tile = tree.tiles.insert_tab_tile(vec![pane_tile]);
        // Tabs::new sets active = first child.
        assert_eq!(active_pane_in_tabs(&tree, tabs_tile), Some(PaneId(1)));
    }

    #[test]
    fn active_pane_in_tabs_returns_explicit_active() {
        let mut tree = Tree::<PaneId>::empty("test-multi");
        let pane_a = tree.tiles.insert_pane(PaneId(10));
        let pane_b = tree.tiles.insert_pane(PaneId(20));
        let tabs_tile = tree.tiles.insert_tab_tile(vec![pane_a, pane_b]);
        if let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) =
            tree.tiles.get_mut(tabs_tile)
        {
            tabs.set_active(pane_b);
        }
        assert_eq!(active_pane_in_tabs(&tree, tabs_tile), Some(PaneId(20)));
    }

    #[test]
    fn active_pane_in_tabs_returns_none_for_empty_tabs() {
        let mut tree = Tree::<PaneId>::empty("test-empty");
        let tabs_tile = tree.tiles.insert_tab_tile(vec![]);
        assert_eq!(active_pane_in_tabs(&tree, tabs_tile), None);
    }

    #[test]
    fn active_pane_in_tabs_returns_none_when_tile_is_not_tabs() {
        let mut tree = Tree::<PaneId>::empty("test-non-tabs");
        let pane_tile = tree.tiles.insert_pane(PaneId(5));
        // The pane tile itself isn't a Tabs container.
        assert_eq!(active_pane_in_tabs(&tree, pane_tile), None);
    }

    #[test]
    fn active_pane_in_tabs_returns_none_for_dangling_tile_id() {
        // `Tree::empty` gives us a tree with no tiles; any `TileId`
        // we manufacture from `Default` won't exist in `tiles`.
        let tree = Tree::<PaneId>::empty("test-dangling");
        assert_eq!(active_pane_in_tabs(&tree, TileId::from_u64(9999)), None);
    }
}
