//! URL detection over the visible terminal grid.
//!
//! Phase 1E-l: walk each viewport row, find `http://` / `https://` /
//! `ftp://` / `file://` URLs by their scheme prefix, and return them
//! as [`LinkSpan`]s anchored in absolute grid coordinates. The
//! eframe app uses this to:
//!
//! - underline the hovered URL when the Cmd/Ctrl modifier is held,
//!   so the user can see what's clickable;
//! - open it via the OS browser on a Cmd/Ctrl-click.
//!
//! ## Out of scope for v1
//!
//! - URLs that wrap across multiple rows because the terminal hit
//!   the right margin. `alacritty_terminal` flags wrapped lines with
//!   [`Flags::WRAPLINE`] — Phase 11 polish can stitch them back
//!   together; for now a wrapped URL only highlights up to the
//!   wrap point, which is the common-case "click on the visible
//!   half" UX.
//! - OSC 8 hyperlinks (`\e]8;…;URL\e\\TEXT\e]8;;\e\\`). The OSC
//!   sniffer in [`crate::osc`] doesn't currently surface these;
//!   they'll arrive with the broader marker pipeline in Phase 3.
//! - Schemeless URLs (`www.example.com`, `example.com/path`). The
//!   scheme requirement keeps false positives low — a row of dots
//!   or a path string never gets picked up as a link.
//!
//! This module is pure: no egui, no PTY, no OS calls. The "open
//! this URL in the user's browser" side of clickable URLs lives at
//! the call site in [`crate::TermicaApp`].

#![forbid(unsafe_code)]

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Cell;

/// One detected URL inside the grid.
///
/// `start` and `end` are inclusive cell positions on the *same* line
/// (multi-row URLs are not unified in v1 — see module docs). `url`
/// is the canonical text we'd hand to the OS opener; we deliberately
/// don't normalise / re-encode it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub start: Point,
    pub end: Point,
    pub url: String,
}

impl LinkSpan {
    /// True when `p` is on this link's line within `[start.column,
    /// end.column]` inclusive.
    pub fn contains(&self, p: Point) -> bool {
        if p.line != self.start.line {
            return false;
        }
        p.column.0 >= self.start.column.0 && p.column.0 <= self.end.column.0
    }
}

/// URL scheme prefixes we recognise. The order matters only for
/// determinism in tests — we look for the *longest* match first when
/// they share a prefix (so `https` wins over `http`).
const URL_SCHEMES: &[&str] = &["https://", "http://", "ftp://", "file://", "mailto:"];

/// Scan the currently visible viewport rows for URLs.
///
/// Returns every URL whose starting cell is on a row that's
/// currently painted. Useful for hover-highlighting and click
/// dispatch — both happen against the rendered viewport, not the
/// scrollback. The caller (the eframe app) re-scans every frame;
/// the cost is `O(visible_rows × visible_cols)` and acceptable
/// at terminal sizes.
pub fn scan_visible_links(grid: &Grid<Cell>) -> Vec<LinkSpan> {
    let display_offset = grid.display_offset() as i32;
    let screen_lines = grid.screen_lines() as i32;
    let cols = grid.columns();
    let mut out = Vec::new();
    for vrow in 0..screen_lines {
        let line = Line(vrow - display_offset);
        let row_chars = collect_row_chars(grid, line, cols);
        for (col_start, col_end, url) in find_urls_in_chars(&row_chars) {
            out.push(LinkSpan {
                start: Point::new(line, Column(col_start)),
                end: Point::new(line, Column(col_end)),
                url,
            });
        }
    }
    out
}

/// Collect one row of the grid as a `Vec<char>`. We index by char,
/// not by byte, because the terminal grid stores one `char` per
/// cell — so column index *is* char index. (Wide glyphs occupy two
/// cells; the second cell stores a spacer, which we keep so that
/// column accounting stays correct.)
fn collect_row_chars(grid: &Grid<Cell>, line: Line, cols: usize) -> Vec<char> {
    let mut v = Vec::with_capacity(cols);
    for c in 0..cols {
        v.push(grid[Point::new(line, Column(c))].c);
    }
    v
}

/// Pure URL scanner. Operates on a row's worth of chars and returns
/// `(col_start, col_end_inclusive, url)` for each URL found.
///
/// Trailing punctuation likely to be sentence terminators is
/// stripped (so `(see https://example.com.)` yields just
/// `https://example.com`). This mirrors what iTerm / Terminal.app /
/// VS Code's terminal all do.
pub fn find_urls_in_chars(chars: &[char]) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if let Some(prefix_len) = match_scheme_at(chars, i) {
            let start = i;
            let mut end = i + prefix_len;
            while end < chars.len() && is_url_char(chars[end]) {
                end += 1;
            }
            // Trim trailing punctuation that's far more often
            // sentence-glue than legitimate URL content.
            while end > start + prefix_len
                && matches!(
                    chars[end - 1],
                    '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"'
                )
            {
                end -= 1;
            }
            // A bare scheme (`http://` with nothing after) isn't a
            // link — only emit when the post-scheme portion has at
            // least one character.
            if end > start + prefix_len {
                let url: String = chars[start..end].iter().collect();
                out.push((start, end - 1, url));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Returns `Some(scheme_len)` if a known URL scheme starts at
/// `chars[i]`, else `None`. Longest-match (`https://` beats
/// `http://`).
fn match_scheme_at(chars: &[char], i: usize) -> Option<usize> {
    for scheme in URL_SCHEMES {
        let s_chars = scheme.chars().count();
        if i + s_chars > chars.len() {
            continue;
        }
        let matches =
            scheme.chars().zip(chars[i..i + s_chars].iter().copied()).all(|(a, b)| a == b);
        if matches {
            return Some(s_chars);
        }
    }
    None
}

/// True when `c` can be part of a URL body. We're deliberately
/// liberal — RFC 3986 allows many characters, and the trailing-
/// punctuation trim in `find_urls_in_chars` catches the common
/// "sentence ends in a URL" case.
fn is_url_char(c: char) -> bool {
    if c.is_whitespace() || c.is_control() {
        return false;
    }
    // Excluded delimiters that the wild web has settled on. Anything
    // else (alpha, digit, `-`, `.`, `/`, `?`, `&`, `=`, `#`, `+`,
    // `%`, `_`, `~`, etc.) is fair game.
    !matches!(c, '<' | '>' | '"' | '`' | '|' | '\\' | '^' | '{' | '}')
}

#[cfg(test)]
mod tests {
    //! Pure tests over `find_urls_in_chars` — no Term, no PTY. The
    //! grid-walking layer (`scan_visible_links`) is exercised by the
    //! same `term_with` helper as `selection.rs`.

    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn urls(s: &str) -> Vec<String> {
        find_urls_in_chars(&chars(s)).into_iter().map(|(_, _, u)| u).collect()
    }

    // --- happy path -----------------------------------------------

    #[test]
    fn detects_plain_https_url() {
        assert_eq!(urls("see https://example.com here"), vec!["https://example.com"]);
    }

    #[test]
    fn detects_http_url() {
        assert_eq!(urls("http://example.com"), vec!["http://example.com"]);
    }

    #[test]
    fn detects_ftp_url() {
        assert_eq!(urls("ftp://files.example.com/dir"), vec!["ftp://files.example.com/dir"]);
    }

    #[test]
    fn detects_file_url() {
        assert_eq!(urls("file:///etc/hosts"), vec!["file:///etc/hosts"]);
    }

    #[test]
    fn detects_two_urls_on_same_row() {
        let u = urls("a https://example.com b https://other.example here");
        assert_eq!(u, vec!["https://example.com", "https://other.example"]);
    }

    #[test]
    fn longest_scheme_match_wins() {
        // `https://` and `http://` both start with `http`; we must
        // pick the long one.
        assert_eq!(urls("https://example.com"), vec!["https://example.com"]);
    }

    #[test]
    fn url_at_start_of_row() {
        assert_eq!(urls("https://example.com is great"), vec!["https://example.com"]);
        let (start, _, _) = find_urls_in_chars(&chars("https://example.com is great"))[0].clone();
        assert_eq!(start, 0);
    }

    #[test]
    fn url_at_end_of_row() {
        let u = urls("visit https://example.com");
        assert_eq!(u, vec!["https://example.com"]);
        let (_, end, _) = find_urls_in_chars(&chars("visit https://example.com"))[0].clone();
        // "visit " is 6 chars; "https://example.com" is 19. Last col is 6+19-1 = 24.
        assert_eq!(end, 24);
    }

    // --- trailing punctuation -------------------------------------

    #[test]
    fn trims_trailing_dot() {
        assert_eq!(urls("see https://example.com."), vec!["https://example.com"]);
    }

    #[test]
    fn trims_trailing_comma() {
        assert_eq!(urls("https://a.example, then..."), vec!["https://a.example"]);
    }

    #[test]
    fn trims_trailing_close_paren() {
        assert_eq!(urls("(see https://example.com)"), vec!["https://example.com"]);
    }

    #[test]
    fn trims_multiple_trailing_chars() {
        assert_eq!(urls("really?! https://a.example?!"), vec!["https://a.example"]);
    }

    // --- query strings / fragments / paths ------------------------

    #[test]
    fn keeps_query_string() {
        assert_eq!(
            urls("https://example.com/path?a=1&b=2"),
            vec!["https://example.com/path?a=1&b=2"]
        );
    }

    #[test]
    fn keeps_fragment() {
        assert_eq!(urls("https://example.com#anchor"), vec!["https://example.com#anchor"]);
    }

    #[test]
    fn keeps_percent_encoded() {
        assert_eq!(urls("https://example.com/a%20b"), vec!["https://example.com/a%20b"]);
    }

    // --- termination ----------------------------------------------

    #[test]
    fn whitespace_terminates_url() {
        assert_eq!(urls("https://a.com https://b.com"), vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn angle_brackets_terminate_url() {
        // Convention for embedding URLs in plaintext: <https://...>
        // The angle brackets are not part of the URL.
        assert_eq!(urls("<https://example.com>"), vec!["https://example.com"]);
    }

    #[test]
    fn quotes_terminate_url() {
        assert_eq!(urls("\"https://example.com\""), vec!["https://example.com"]);
    }

    // --- false positives -----------------------------------------

    #[test]
    fn schemeless_url_is_not_detected() {
        // We intentionally only match URLs with an explicit scheme.
        assert!(urls("example.com").is_empty());
        assert!(urls("www.example.com").is_empty());
    }

    #[test]
    fn lone_scheme_is_not_a_url() {
        // `http://` on its own has nothing to open.
        assert!(urls("http://").is_empty());
        assert!(urls("https://").is_empty());
    }

    // --- scan_visible_links ---------------------------------------

    use alacritty_terminal::Term;
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    #[derive(Default)]
    struct NopListener;
    impl EventListener for NopListener {
        fn send_event(&self, _e: Event) {}
    }

    fn term_with(text: &[u8], rows: u16, cols: u16) -> Term<NopListener> {
        let size = TermSize::new(cols as usize, rows as usize);
        let mut term = Term::new(Config::default(), &size, NopListener);
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        for b in text {
            parser.advance(&mut term, &[*b]);
        }
        term
    }

    #[test]
    fn scan_visible_links_finds_url_in_grid() {
        // Two URLs on separate rows of the live viewport.
        let term = term_with(b"see https://example.com\r\nor http://other.example", 5, 60);
        let spans = scan_visible_links(term.grid());
        assert_eq!(spans.len(), 2);

        let urls: Vec<_> = spans.iter().map(|s| s.url.as_str()).collect();
        assert!(urls.contains(&"https://example.com"));
        assert!(urls.contains(&"http://other.example"));

        // Row indices: line 0 has the first, line 1 the second.
        let lines: Vec<i32> = spans.iter().map(|s| s.start.line.0).collect();
        assert!(lines.contains(&0));
        assert!(lines.contains(&1));
    }

    #[test]
    fn scan_visible_links_returns_empty_when_no_urls() {
        let term = term_with(b"just plain text here", 5, 40);
        assert!(scan_visible_links(term.grid()).is_empty());
    }

    // --- contains -------------------------------------------------

    #[test]
    fn link_contains_point_only_on_correct_line_within_range() {
        let link = LinkSpan {
            start: Point::new(Line(2), Column(3)),
            end: Point::new(Line(2), Column(10)),
            url: "https://x.example".to_string(),
        };
        assert!(link.contains(Point::new(Line(2), Column(3))));
        assert!(link.contains(Point::new(Line(2), Column(10))));
        assert!(link.contains(Point::new(Line(2), Column(7))));
        // Wrong line.
        assert!(!link.contains(Point::new(Line(1), Column(7))));
        assert!(!link.contains(Point::new(Line(3), Column(7))));
        // Out of column range.
        assert!(!link.contains(Point::new(Line(2), Column(2))));
        assert!(!link.contains(Point::new(Line(2), Column(11))));
    }
}
