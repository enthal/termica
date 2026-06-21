//! Enforces the "Markdown is never hard-wrapped" policy (CLAUDE.md →
//! Code style): one logical line per paragraph / list item /
//! block-quote. A hard wrap is a line that *continues* the previous
//! line of the same block — the renderer joins them anyway, so the
//! line breaks carry no meaning and just churn diffs.
//!
//! Pure, dependency-free, and run by the existing CI `test` job — no
//! Node/prettier toolchain. The fixture battery below is the proof
//! that it has neither false positives (structural blocks, lists,
//! tables, code, HTML, single-line badges/block-quotes are all fine)
//! nor false negatives (wrapped paragraphs, list items, block-quotes,
//! and multi-line badge paragraphs are all caught).

use std::path::{Path, PathBuf};

/// 1-based line numbers that hard-wrap (continue) a previous line of
/// the same block. Empty == the file is soft-wrapped.
fn hard_wrap_violations(content: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut in_code = false;
    // Is a paragraph / list item / block-quote currently "open" — i.e.
    // would a bare text line on the next row continue it (= hard wrap)?
    let mut open = false;

    for (i, raw) in content.lines().enumerate() {
        let n = i + 1;
        let t = raw.trim_start();

        // Fenced code: toggle and never inspect the contents.
        if t.starts_with("```") || t.starts_with("~~~") {
            in_code = !in_code;
            open = false;
            continue;
        }
        if in_code {
            open = false;
            continue;
        }

        // Inspect block-quote content under the same rules by peeling
        // the `>` markers — a one-line quote is fine, a wrapped one is
        // caught.
        let body = strip_blockquote(t).trim_start();

        if body.is_empty() {
            open = false; // blank line (or empty `>`): block boundary
            continue;
        }

        // Block starters never flow into a previous line and a bare
        // line after them is a *new* block, not a continuation.
        if is_block_starter(body) {
            open = false;
            continue;
        }

        // A list item starts its own logical line: it doesn't continue
        // the previous one, but it can itself be continued.
        if is_list_item(body) {
            open = true;
            continue;
        }

        // Plain text. If a block is already open, this row continues it.
        if open {
            out.push(n);
        }
        open = true;
    }
    out
}

fn strip_blockquote(s: &str) -> &str {
    let mut s = s;
    loop {
        let t = s.trim_start();
        match t.strip_prefix('>') {
            Some(rest) => s = rest.strip_prefix(' ').unwrap_or(rest),
            None => return s,
        }
    }
}

fn is_block_starter(b: &str) -> bool {
    b.starts_with('#')      // ATX heading
        || b.starts_with('<') // HTML block
        || b.contains('|')    // table row (or a nav line); never flowing prose here
        || is_thematic_break(b)
}

/// `---` / `***` / `___` rules and `===` / `---` setext underlines.
fn is_thematic_break(b: &str) -> bool {
    let t = b.trim_end();
    let Some(c0) = t.chars().next() else { return false };
    if !matches!(c0, '-' | '*' | '_' | '=') {
        return false;
    }
    t.chars().all(|c| c == c0 || c == ' ') && t.chars().filter(|&c| c == c0).count() >= 3
}

fn is_list_item(b: &str) -> bool {
    if b.starts_with("- ") || b.starts_with("* ") || b.starts_with("+ ") {
        return true;
    }
    // Ordered: digits then `.` or `)` then a space (or end of line).
    let bytes = b.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    i > 0
        && i < bytes.len()
        && (bytes[i] == b'.' || bytes[i] == b')')
        && (i + 1 == bytes.len() || bytes[i + 1] == b' ')
}

// ---- the policy enforcement over the whole repo -------------------

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            // Skip build output, VCS, agent scratch, vendored deps.
            if matches!(name.to_str(), Some("target" | ".git" | ".claude" | "node_modules")) {
                continue;
            }
            collect_markdown(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

#[test]
fn no_markdown_is_hard_wrapped() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_markdown(root, &mut files);
    files.sort();
    assert!(!files.is_empty(), "found no markdown files to check under {}", root.display());

    let mut report = String::new();
    for file in &files {
        let content = std::fs::read_to_string(file).expect("read markdown");
        let bad = hard_wrap_violations(&content);
        if !bad.is_empty() {
            let rel = file.strip_prefix(root).unwrap_or(file);
            report.push_str(&format!("  {}: hard-wrapped lines {:?}\n", rel.display(), bad));
        }
    }
    assert!(
        report.is_empty(),
        "Markdown must never be hard-wrapped (one line per paragraph). Offending lines:\n{report}"
    );
}

// ---- false-positive / false-negative proof ------------------------

#[test]
fn soft_wrapped_constructs_are_clean() {
    // Each of these is correctly NOT a hard wrap.
    let cases: &[(&str, &str)] = &[
        ("one-line paragraph", "Single flowing line, however long it is.\n"),
        ("blank-separated paras", "First paragraph.\n\nSecond paragraph.\n"),
        ("heading then para", "# Title\n\nBody paragraph.\n"),
        ("list items", "- first item\n- second item\n- third item\n"),
        ("ordered list", "1. first\n2. second\n"),
        ("nested list", "- parent item\n  - child item\n  - other child\n"),
        ("table", "| A | B |\n| --- | --- |\n| 1 | 2 |\n"),
        ("fenced code with prose", "```\nthis looks like\nprose but is code\n```\n"),
        ("html block", "<div align=\"center\">\n<img src=\"x.png\" />\n</div>\n"),
        ("badges on one line", "[![A](u)](v) [![B](u)](v) [![C](u)](v)\n"),
        ("one-line blockquote", "> A single-line note.\n"),
        ("thematic break", "Para.\n\n---\n\nNext para.\n"),
        ("nav line with pipe", "**Prev:** [a](a.md) | **Next:** [b](b.md)\n"),
    ];
    for (name, md) in cases {
        assert!(
            hard_wrap_violations(md).is_empty(),
            "false positive on `{name}`: flagged {:?}\n---\n{md}",
            hard_wrap_violations(md)
        );
    }
}

#[test]
fn hard_wrapped_constructs_are_caught() {
    // (markdown, the lines that MUST be flagged)
    let cases: &[(&str, &str, &[usize])] = &[
        ("wrapped paragraph", "A paragraph that runs on\nand wraps to a second line.\n", &[2]),
        (
            "three-line wrap",
            "Line one of the thought,\nline two of it,\nand line three.\n",
            &[2, 3],
        ),
        (
            "wrapped list item",
            "- an item whose text is long\n  and wraps onto the next row\n",
            &[2],
        ),
        ("wrapped ordered item", "1. a numbered item that\n   continues below\n", &[2]),
        ("wrapped blockquote", "> a quoted note that\n> wraps to a second line\n", &[2]),
        ("multi-line badge paragraph", "[![A](u)](v)\n[![B](u)](v)\n[![C](u)](v)\n", &[2, 3]),
        (
            "wrap after a heading",
            "# Title\n\nThis paragraph wraps\nacross two physical lines.\n",
            &[4],
        ),
    ];
    for (name, md, expected) in cases {
        assert_eq!(&hard_wrap_violations(md), expected, "false negative on `{name}`\n---\n{md}");
    }
}
