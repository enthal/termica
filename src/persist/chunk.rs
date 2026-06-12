//! Scrollback chunk format (Phase 9A).
//!
//! A *chunk* is an append-only file holding a contiguous range of a
//! pane's transcript, stored as **logical lines** — what the program
//! emitted between hard newlines, width-independent — rather than grid
//! rows at a fixed width. Storing logical lines is what lets sealed
//! blocks reflow on resize and restore cleanly into a differently
//! sized window (see [spec/08 §"Logical lines, not grid rows"](../../spec/08-persistence.md#logical-lines-not-grid-rows)).
//!
//! This module owns three responsibilities, each separately tested:
//!
//! 1. **A faithful codec** — [`encode_chunk`] / [`decode_chunk`] turn a
//!    `&[StyledLine]` into the wire format and back, preserving every
//!    char, color, and flag. Optional zstd compression wraps only the
//!    record body; the 16-byte header is never compressed.
//! 2. **A Termica-owned color encoding** — [`ChunkColor`] decouples the
//!    stored bytes from `alacritty_terminal`'s `Color` enum, so an
//!    upstream change to that type cannot silently break stored chunks.
//! 3. **The grid → logical transform** — [`unwrap_rows`] joins soft-
//!    wrapped grid rows into logical lines, drops wide-char spacer
//!    cells, strips the `WRAPLINE` marker, and trims trailing padding.
//!
//! Wrapping back to grid rows at a given width (`wrap_logical_line`) is
//! the *render* direction and lands in 9B; this slice is pure storage.
//!
//! ## Wire format
//!
//! ```text
//! header (16 bytes):
//!   magic    : "TMCK"   (4)
//!   version  : u32 le   (4)
//!   flags    : u32 le   (4)   -- bit 0 = body is zstd-compressed
//!   reserved : u32 le   (4)
//!
//! body (zstd-compressed iff flags bit 0; header never compressed):
//!   record* :
//!     line_len   : u32 le        -- byte length of line_bytes
//!     style_len  : u32 le        -- byte length of style_bytes
//!     line_bytes : UTF-8         -- the logical line's chars, in order
//!     style_bytes:
//!       run_count : u32 le
//!       run_count × { run_len: u32 le, fg: color, bg: color, flags: u16 le }
//!
//! color: tag u8 + payload
//!   0x00 Named   : u16 le   -- NamedColor discriminant (0..15, 256..)
//!   0x01 Rgb     : u8 u8 u8
//!   0x02 Indexed : u8
//! ```
//!
//! The per-line cell count is recovered from the style runs (one cell
//! per run-length unit), not from `line_bytes` length, since a char may
//! be multi-byte. The char count and the expanded run count must agree
//! or decode fails — this equality is the codec's round-trip invariant.

#![forbid(unsafe_code)]

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::terminal::{StyledCell, StyledLine};

/// File magic: the first four bytes of every chunk.
pub const CHUNK_MAGIC: [u8; 4] = *b"TMCK";
/// Wire-format version. Bumped only on an incompatible layout change.
pub const CHUNK_VERSION: u32 = 1;
/// `flags` bit 0: the record body is zstd-compressed.
pub const FLAG_COMPRESSED: u32 = 1 << 0;
/// zstd compression level used on seal. Level 3 is zstd's default —
/// a good speed/ratio balance for text, and the seal path runs off the
/// UI thread anyway.
const ZSTD_LEVEL: i32 = 3;

/// Color as stored in a chunk — Termica's own encoding, deliberately
/// independent of [`alacritty_terminal::vte::ansi::Color`] so that an
/// upstream change to that enum's shape cannot silently corrupt the
/// meaning of bytes already on disk. The conversions in [`Self::from_ansi`]
/// / [`Self::to_ansi`] are the only bridge, and are round-trip tested
/// over every [`NamedColor`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkColor {
    /// One of the terminal's named colors, stored as its discriminant.
    Named(u16),
    /// A 24-bit true-color value.
    Rgb(u8, u8, u8),
    /// A 256-color palette index.
    Indexed(u8),
}

impl ChunkColor {
    /// Lossless projection of an alacritty color onto the stored form.
    pub fn from_ansi(c: Color) -> Self {
        match c {
            // `NamedColor` is a field-less enum; `as u16` yields its
            // discriminant (0..15 for the ANSI 16, 256.. for the
            // semantic colors like Foreground / Background).
            Color::Named(n) => ChunkColor::Named(n as u16),
            Color::Spec(rgb) => ChunkColor::Rgb(rgb.r, rgb.g, rgb.b),
            Color::Indexed(i) => ChunkColor::Indexed(i),
        }
    }

    /// Inverse of [`Self::from_ansi`]. Errors on a named discriminant
    /// we don't recognise (a corrupt or forward-version chunk) rather
    /// than guessing a color.
    pub fn to_ansi(self) -> Result<Color, ChunkError> {
        Ok(match self {
            ChunkColor::Named(d) => {
                Color::Named(named_from_u16(d).ok_or(ChunkError::UnknownNamedColor(d))?)
            }
            ChunkColor::Rgb(r, g, b) => Color::Spec(Rgb { r, g, b }),
            ChunkColor::Indexed(i) => Color::Indexed(i),
        })
    }
}

/// Map a stored discriminant back to a [`NamedColor`]. Exhaustive over
/// the variants of `vte`'s enum; an unknown value returns `None` (the
/// caller surfaces [`ChunkError::UnknownNamedColor`]). Kept as an
/// explicit table — not a transmute — so it is safe and so the
/// round-trip test catches any upstream reordering.
fn named_from_u16(d: u16) -> Option<NamedColor> {
    use NamedColor::*;
    Some(match d {
        0 => Black,
        1 => Red,
        2 => Green,
        3 => Yellow,
        4 => Blue,
        5 => Magenta,
        6 => Cyan,
        7 => White,
        8 => BrightBlack,
        9 => BrightRed,
        10 => BrightGreen,
        11 => BrightYellow,
        12 => BrightBlue,
        13 => BrightMagenta,
        14 => BrightCyan,
        15 => BrightWhite,
        256 => Foreground,
        257 => Background,
        258 => Cursor,
        259 => DimBlack,
        260 => DimRed,
        261 => DimGreen,
        262 => DimYellow,
        263 => DimBlue,
        264 => DimMagenta,
        265 => DimCyan,
        266 => DimWhite,
        267 => BrightForeground,
        268 => DimForeground,
        _ => return None,
    })
}

/// Everything that can go wrong decoding a chunk. All are corruption /
/// truncation / version cases — a well-formed chunk written by
/// [`encode_chunk`] never produces one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    /// The first four bytes were not `"TMCK"`.
    #[error("bad chunk magic (not TMCK)")]
    BadMagic,
    /// The header declared a version this build can't read.
    #[error("unsupported chunk version {0}")]
    BadVersion(u32),
    /// Ran out of bytes mid-field.
    #[error("chunk truncated")]
    Truncated,
    /// Trailing bytes remained after the last complete record.
    #[error("trailing garbage after last chunk record")]
    TrailingBytes,
    /// A color tag byte was not one of `0x00..=0x02`.
    #[error("invalid color tag {0:#x}")]
    BadColorTag(u8),
    /// A named-color discriminant outside the known set.
    #[error("unknown named-color discriminant {0}")]
    UnknownNamedColor(u16),
    /// `line_bytes` was not valid UTF-8.
    #[error("line bytes are not valid UTF-8")]
    BadUtf8,
    /// The char count and the expanded style-run cell count disagreed.
    #[error("cell-count mismatch: {chars} chars but {cells} styled cells")]
    CellCountMismatch { chars: usize, cells: usize },
    /// zstd refused to inflate the body.
    #[error("zstd decompression failed")]
    Decompress,
}

// --- encode ---------------------------------------------------------

/// Serialize logical `lines` into a chunk. When `compress` is set the
/// record body is zstd-compressed and `FLAG_COMPRESSED` is recorded;
/// the 16-byte header is always plain so a reader can route on it
/// without inflating anything.
///
/// `lines` are expected to be logical lines (as produced by
/// [`unwrap_rows`]); the codec is faithful, so it round-trips whatever
/// cells it is given, but the *format's* "no wrap markers" property
/// comes from the data, not from masking here.
pub fn encode_chunk(lines: &[StyledLine], compress: bool) -> Vec<u8> {
    let mut body = Vec::new();
    for line in lines {
        encode_logical_line(&mut body, line);
    }

    let mut flags = 0u32;
    let body = if compress {
        flags |= FLAG_COMPRESSED;
        // `&[u8]: Read`, so encode_all reads the whole body. zstd only
        // errors here on allocation failure, which we treat as fatal
        // (encode is infallible by contract) — fall back to raw.
        match zstd::stream::encode_all(body.as_slice(), ZSTD_LEVEL) {
            Ok(compressed) => compressed,
            Err(_) => {
                flags &= !FLAG_COMPRESSED;
                body
            }
        }
    } else {
        body
    };

    let mut out = Vec::with_capacity(16 + body.len());
    out.extend_from_slice(&CHUNK_MAGIC);
    out.extend_from_slice(&CHUNK_VERSION.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved
    out.extend_from_slice(&body);
    out
}

fn encode_logical_line(out: &mut Vec<u8>, line: &StyledLine) {
    let line_bytes: Vec<u8> = line.cells.iter().map(|c| c.c).collect::<String>().into_bytes();

    let mut style_bytes = Vec::new();
    encode_style_runs(&mut style_bytes, &line.cells);

    out.extend_from_slice(&(line_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&(style_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&line_bytes);
    out.extend_from_slice(&style_bytes);
}

/// Run-length-encode the per-cell `(fg, bg, flags)` style. The char
/// content is carried separately in `line_bytes`; here we only collapse
/// runs of identical style.
fn encode_style_runs(out: &mut Vec<u8>, cells: &[StyledCell]) {
    // Collect runs first so we can prefix the count.
    let mut runs: Vec<(u32, ChunkColor, ChunkColor, u16)> = Vec::new();
    for cell in cells {
        let key =
            (ChunkColor::from_ansi(cell.fg), ChunkColor::from_ansi(cell.bg), cell.flags.bits());
        match runs.last_mut() {
            Some((len, fg, bg, flags)) if (*fg, *bg, *flags) == key => *len += 1,
            _ => runs.push((1, key.0, key.1, key.2)),
        }
    }

    out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
    for (len, fg, bg, flags) in runs {
        out.extend_from_slice(&len.to_le_bytes());
        encode_color(out, fg);
        encode_color(out, bg);
        out.extend_from_slice(&flags.to_le_bytes());
    }
}

fn encode_color(out: &mut Vec<u8>, c: ChunkColor) {
    match c {
        ChunkColor::Named(d) => {
            out.push(0x00);
            out.extend_from_slice(&d.to_le_bytes());
        }
        ChunkColor::Rgb(r, g, b) => {
            out.push(0x01);
            out.push(r);
            out.push(g);
            out.push(b);
        }
        ChunkColor::Indexed(i) => {
            out.push(0x02);
            out.push(i);
        }
    }
}

// --- decode ---------------------------------------------------------

/// Parse a chunk produced by [`encode_chunk`] back into its logical
/// lines. Validates magic, version, framing, UTF-8, and the
/// char/cell-count equality.
pub fn decode_chunk(bytes: &[u8]) -> Result<Vec<StyledLine>, ChunkError> {
    let mut cur = Cursor::new(bytes);
    let magic = cur.take(4)?;
    if magic != CHUNK_MAGIC {
        return Err(ChunkError::BadMagic);
    }
    let version = cur.u32()?;
    if version != CHUNK_VERSION {
        return Err(ChunkError::BadVersion(version));
    }
    let flags = cur.u32()?;
    let _reserved = cur.u32()?;

    // Body is the rest; inflate if compressed.
    let body_owned;
    let body: &[u8] = if flags & FLAG_COMPRESSED != 0 {
        body_owned = zstd::stream::decode_all(cur.rest()).map_err(|_| ChunkError::Decompress)?;
        &body_owned
    } else {
        cur.rest()
    };

    let mut body_cur = Cursor::new(body);
    let mut lines = Vec::new();
    while !body_cur.is_empty() {
        lines.push(decode_logical_line(&mut body_cur)?);
    }
    Ok(lines)
}

fn decode_logical_line(cur: &mut Cursor<'_>) -> Result<StyledLine, ChunkError> {
    let line_len = cur.u32()? as usize;
    let style_len = cur.u32()? as usize;
    let line_bytes = cur.take(line_len)?;
    let style_bytes = cur.take(style_len)?;

    let text = std::str::from_utf8(line_bytes).map_err(|_| ChunkError::BadUtf8)?;
    let chars: Vec<char> = text.chars().collect();

    // Expand style runs into one style per cell.
    let mut style_cur = Cursor::new(style_bytes);
    let run_count = style_cur.u32()? as usize;
    let mut styles: Vec<(Color, Color, Flags)> = Vec::new();
    for _ in 0..run_count {
        let run_len = style_cur.u32()? as usize;
        let fg = read_color(&mut style_cur)?.to_ansi()?;
        let bg = read_color(&mut style_cur)?.to_ansi()?;
        let flags = Flags::from_bits_truncate(style_cur.u16()?);
        for _ in 0..run_len {
            styles.push((fg, bg, flags));
        }
    }
    if !style_cur.is_empty() {
        return Err(ChunkError::TrailingBytes);
    }

    if chars.len() != styles.len() {
        return Err(ChunkError::CellCountMismatch { chars: chars.len(), cells: styles.len() });
    }

    let cells = chars
        .into_iter()
        .zip(styles)
        .map(|(c, (fg, bg, flags))| StyledCell { c, fg, bg, flags })
        .collect();
    Ok(StyledLine { cells })
}

fn read_color(cur: &mut Cursor<'_>) -> Result<ChunkColor, ChunkError> {
    let tag = cur.u8()?;
    Ok(match tag {
        0x00 => ChunkColor::Named(cur.u16()?),
        0x01 => {
            let r = cur.u8()?;
            let g = cur.u8()?;
            let b = cur.u8()?;
            ChunkColor::Rgb(r, g, b)
        }
        0x02 => ChunkColor::Indexed(cur.u8()?),
        other => return Err(ChunkError::BadColorTag(other)),
    })
}

/// Minimal forward-only byte reader. Every read is bounds-checked and
/// maps an overrun to [`ChunkError::Truncated`], so decode never
/// panics on malformed input.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn rest(&self) -> &'a [u8] {
        &self.bytes[self.pos.min(self.bytes.len())..]
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ChunkError> {
        let end = self.pos.checked_add(n).ok_or(ChunkError::Truncated)?;
        if end > self.bytes.len() {
            return Err(ChunkError::Truncated);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ChunkError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ChunkError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ChunkError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

// --- grid rows -> logical lines -------------------------------------

/// Collapse grid `rows` into width-independent logical lines, ready to
/// hand to [`encode_chunk`]. This is the un-wrap half of reflow:
///
/// - Consecutive rows are **joined** where the earlier row's last cell
///   carries [`Flags::WRAPLINE`] (alacritty's soft-wrap marker); a row
///   whose last cell lacks it ends a logical line.
/// - Wide-char **spacer** cells ([`Flags::WIDE_CHAR_SPACER`] /
///   [`Flags::LEADING_WIDE_CHAR_SPACER`]) are dropped — they are pure
///   width artifacts, re-derived when wrapping back to a width in 9B.
///   The wide char itself (carrying [`Flags::WIDE_CHAR`]) is kept.
/// - The `WRAPLINE` marker is stripped from retained cells, since the
///   join has consumed its meaning.
/// - Trailing **default-blank** padding (a space with default fg/bg and
///   no flags — the grid's right-margin fill) is trimmed once per
///   logical line. A trailing space carrying a non-default background
///   (e.g. a colored bar) is real content and is kept.
pub fn unwrap_rows(rows: &[StyledLine]) -> Vec<StyledLine> {
    let mut out = Vec::new();
    let mut current: Vec<StyledCell> = Vec::new();

    for row in rows {
        let continues = row.cells.last().is_some_and(|c| c.flags.contains(Flags::WRAPLINE));

        for cell in &row.cells {
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut kept = cell.clone();
            kept.flags.remove(Flags::WRAPLINE);
            current.push(kept);
        }

        if !continues {
            trim_trailing_default(&mut current);
            out.push(StyledLine { cells: std::mem::take(&mut current) });
        }
    }

    // A final row that ended in WRAPLINE with no continuation: flush
    // what we have rather than dropping it.
    if !current.is_empty() {
        trim_trailing_default(&mut current);
        out.push(StyledLine { cells: current });
    }

    out
}

/// True for a cell that is exactly the terminal's default padding cell:
/// a space, default foreground and background, no flags. These are the
/// cells the grid fills the right margin with.
fn is_default_blank(cell: &StyledCell) -> bool {
    cell.c == ' '
        && cell.flags.is_empty()
        && matches!(cell.fg, Color::Named(NamedColor::Foreground))
        && matches!(cell.bg, Color::Named(NamedColor::Background))
}

fn trim_trailing_default(cells: &mut Vec<StyledCell>) {
    while cells.last().is_some_and(is_default_blank) {
        cells.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_NAMED: &[NamedColor] = &[
        NamedColor::Black,
        NamedColor::Red,
        NamedColor::Green,
        NamedColor::Yellow,
        NamedColor::Blue,
        NamedColor::Magenta,
        NamedColor::Cyan,
        NamedColor::White,
        NamedColor::BrightBlack,
        NamedColor::BrightRed,
        NamedColor::BrightGreen,
        NamedColor::BrightYellow,
        NamedColor::BrightBlue,
        NamedColor::BrightMagenta,
        NamedColor::BrightCyan,
        NamedColor::BrightWhite,
        NamedColor::Foreground,
        NamedColor::Background,
        NamedColor::Cursor,
        NamedColor::DimBlack,
        NamedColor::DimRed,
        NamedColor::DimGreen,
        NamedColor::DimYellow,
        NamedColor::DimBlue,
        NamedColor::DimMagenta,
        NamedColor::DimCyan,
        NamedColor::DimWhite,
        NamedColor::BrightForeground,
        NamedColor::DimForeground,
    ];

    fn fg_default() -> Color {
        Color::Named(NamedColor::Foreground)
    }
    fn bg_default() -> Color {
        Color::Named(NamedColor::Background)
    }

    /// A plain default-styled cell.
    fn cell(c: char) -> StyledCell {
        StyledCell { c, fg: fg_default(), bg: bg_default(), flags: Flags::empty() }
    }

    /// A styled cell with explicit fg / bg / flags.
    fn styled(c: char, fg: Color, bg: Color, flags: Flags) -> StyledCell {
        StyledCell { c, fg, bg, flags }
    }

    /// A logical line of default-styled cells from a string.
    fn line(s: &str) -> StyledLine {
        StyledLine { cells: s.chars().map(cell).collect() }
    }

    // ---- color encoding ---------------------------------------------

    #[test]
    fn named_color_roundtrips_every_variant() {
        for &n in ALL_NAMED {
            let cc = ChunkColor::from_ansi(Color::Named(n));
            let back = cc.to_ansi().expect("known named color round-trips");
            assert_eq!(back, Color::Named(n), "variant {n:?} must survive round-trip");
            // And the discriminant maps back exactly.
            if let ChunkColor::Named(d) = cc {
                assert_eq!(named_from_u16(d), Some(n));
            } else {
                panic!("named color did not encode as Named");
            }
        }
    }

    #[test]
    fn rgb_and_indexed_roundtrip() {
        for c in [
            Color::Spec(Rgb { r: 0, g: 0, b: 0 }),
            Color::Spec(Rgb { r: 255, g: 128, b: 1 }),
            Color::Indexed(0),
            Color::Indexed(231),
            Color::Indexed(255),
        ] {
            assert_eq!(ChunkColor::from_ansi(c).to_ansi().unwrap(), c);
        }
    }

    #[test]
    fn color_byte_encoding_roundtrips() {
        for c in [ChunkColor::Named(256), ChunkColor::Rgb(9, 8, 7), ChunkColor::Indexed(42)] {
            let mut buf = Vec::new();
            encode_color(&mut buf, c);
            let mut cur = Cursor::new(&buf);
            assert_eq!(read_color(&mut cur).unwrap(), c);
            assert!(cur.is_empty(), "color decode must consume exactly its bytes");
        }
    }

    #[test]
    fn unknown_named_discriminant_errors() {
        assert_eq!(ChunkColor::Named(9999).to_ansi(), Err(ChunkError::UnknownNamedColor(9999)));
    }

    // ---- codec round-trips ------------------------------------------

    fn assert_roundtrip(lines: &[StyledLine]) {
        for compress in [false, true] {
            let bytes = encode_chunk(lines, compress);
            assert_eq!(&bytes[0..4], &CHUNK_MAGIC);
            let decoded = decode_chunk(&bytes).expect("valid chunk decodes");
            assert_eq!(decoded, lines, "compress={compress}");
        }
    }

    #[test]
    fn single_line_roundtrip() {
        assert_roundtrip(&[line("hello world")]);
    }

    #[test]
    fn multiline_mixed_styles_roundtrip() {
        let lines = vec![
            StyledLine {
                cells: vec![
                    styled('E', Color::Named(NamedColor::Red), bg_default(), Flags::BOLD),
                    styled('r', Color::Named(NamedColor::Red), bg_default(), Flags::BOLD),
                    styled('r', Color::Indexed(196), bg_default(), Flags::empty()),
                    styled('!', Color::Spec(Rgb { r: 1, g: 2, b: 3 }), bg_default(), Flags::ITALIC),
                ],
            },
            line("plain"),
            StyledLine {
                cells: vec![styled(
                    '█',
                    Color::Spec(Rgb { r: 10, g: 20, b: 30 }),
                    Color::Named(NamedColor::Blue),
                    Flags::UNDERLINE | Flags::DIM,
                )],
            },
        ];
        assert_roundtrip(&lines);
    }

    #[test]
    fn empty_chunk_and_blank_lines_roundtrip() {
        assert_roundtrip(&[]);
        assert_roundtrip(&[StyledLine::default()]); // a single empty logical line
        assert_roundtrip(&[StyledLine::default(), line("x"), StyledLine::default()]);
    }

    #[test]
    fn multibyte_chars_roundtrip() {
        // Cell count comes from style runs, not byte length, so multi-
        // byte chars must survive even though line_len != cell count.
        assert_roundtrip(&[line("café — 日本語 🦀")]);
    }

    #[test]
    fn compressed_sets_flag_and_shrinks_repetitive_body() {
        let big = StyledLine { cells: std::iter::repeat_n(cell('a'), 4096).collect() };
        let raw = encode_chunk(std::slice::from_ref(&big), false);
        let zipped = encode_chunk(std::slice::from_ref(&big), true);
        let flags = u32::from_le_bytes([zipped[8], zipped[9], zipped[10], zipped[11]]);
        assert_eq!(flags & FLAG_COMPRESSED, FLAG_COMPRESSED, "compressed flag must be set");
        assert_eq!(raw[8..12], [0, 0, 0, 0], "raw chunk must not set the flag");
        assert!(zipped.len() < raw.len(), "zstd must shrink a highly repetitive body");
    }

    // ---- decode error paths -----------------------------------------

    #[test]
    fn bad_magic_errors() {
        let mut bytes = encode_chunk(&[line("x")], false);
        bytes[0] = b'X';
        assert_eq!(decode_chunk(&bytes), Err(ChunkError::BadMagic));
    }

    #[test]
    fn bad_version_errors() {
        let mut bytes = encode_chunk(&[line("x")], false);
        bytes[4] = 99;
        assert_eq!(decode_chunk(&bytes), Err(ChunkError::BadVersion(99)));
    }

    #[test]
    fn truncated_header_errors() {
        assert_eq!(decode_chunk(b"TMC"), Err(ChunkError::Truncated));
        assert_eq!(decode_chunk(&CHUNK_MAGIC), Err(ChunkError::Truncated));
    }

    #[test]
    fn truncated_body_errors() {
        let bytes = encode_chunk(&[line("hello")], false);
        // Drop the last byte of a record's payload.
        assert_eq!(decode_chunk(&bytes[..bytes.len() - 1]), Err(ChunkError::Truncated));
    }

    #[test]
    fn cell_count_mismatch_errors() {
        // Hand-build a record whose 3 chars disagree with a 2-cell run.
        let mut body = Vec::new();
        let line_bytes = b"abc";
        let mut style = Vec::new();
        style.extend_from_slice(&1u32.to_le_bytes()); // run_count = 1
        style.extend_from_slice(&2u32.to_le_bytes()); // run_len = 2 (!= 3 chars)
        encode_color(&mut style, ChunkColor::Named(256));
        encode_color(&mut style, ChunkColor::Named(257));
        style.extend_from_slice(&0u16.to_le_bytes()); // flags
        body.extend_from_slice(&(line_bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&(style.len() as u32).to_le_bytes());
        body.extend_from_slice(line_bytes);
        body.extend_from_slice(&style);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&CHUNK_MAGIC);
        bytes.extend_from_slice(&CHUNK_VERSION.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&body);

        assert_eq!(decode_chunk(&bytes), Err(ChunkError::CellCountMismatch { chars: 3, cells: 2 }));
    }

    // ---- property: random lines round-trip --------------------------

    /// Deterministic xorshift64 — no `rand` crate, fixed seed, so the
    /// property test is reproducible (the project forbids unseeded
    /// randomness in tests).
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn property_random_lines_roundtrip() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let palette_chars = ['a', 'b', 'Z', '0', ' ', '日', '🦀', '#'];
        let flag_pool = [
            Flags::empty(),
            Flags::BOLD,
            Flags::ITALIC,
            Flags::UNDERLINE,
            Flags::DIM,
            Flags::INVERSE,
            Flags::STRIKEOUT,
            Flags::BOLD | Flags::UNDERLINE,
        ];

        for _ in 0..200 {
            let line_count = rng.below(6) as usize;
            let mut lines = Vec::with_capacity(line_count);
            for _ in 0..line_count {
                let cell_count = rng.below(24) as usize;
                let mut cells = Vec::with_capacity(cell_count);
                for _ in 0..cell_count {
                    let c = palette_chars[rng.below(palette_chars.len() as u64) as usize];
                    let fg = match rng.below(3) {
                        0 => Color::Named(ALL_NAMED[rng.below(ALL_NAMED.len() as u64) as usize]),
                        1 => Color::Indexed(rng.below(256) as u8),
                        _ => Color::Spec(Rgb {
                            r: rng.below(256) as u8,
                            g: rng.below(256) as u8,
                            b: rng.below(256) as u8,
                        }),
                    };
                    let bg = match rng.below(2) {
                        0 => bg_default(),
                        _ => Color::Indexed(rng.below(256) as u8),
                    };
                    let flags = flag_pool[rng.below(flag_pool.len() as u64) as usize];
                    cells.push(StyledCell { c, fg, bg, flags });
                }
                lines.push(StyledLine { cells });
            }

            for compress in [false, true] {
                let bytes = encode_chunk(&lines, compress);
                assert_eq!(decode_chunk(&bytes).unwrap(), lines);
            }
        }
    }

    // ---- unwrap: grid rows -> logical lines --------------------------

    /// Build a row and set WRAPLINE on its last cell (alacritty's soft-
    /// wrap marker).
    fn wrapped_row(s: &str) -> StyledLine {
        let mut l = line(s);
        if let Some(last) = l.cells.last_mut() {
            last.flags.insert(Flags::WRAPLINE);
        }
        l
    }

    #[test]
    fn unwrap_joins_soft_wrapped_rows() {
        let rows = vec![wrapped_row("abcde"), line("fg")];
        let logical = unwrap_rows(&rows);
        assert_eq!(logical.len(), 1);
        let text: String = logical[0].text_chars().collect();
        assert_eq!(text, "abcdefg");
    }

    #[test]
    fn unwrap_keeps_hard_newlines_separate() {
        let rows = vec![line("ab"), line("cd")];
        let logical = unwrap_rows(&rows);
        assert_eq!(logical.len(), 2);
        assert_eq!(logical[0].text_chars().collect::<String>(), "ab");
        assert_eq!(logical[1].text_chars().collect::<String>(), "cd");
    }

    #[test]
    fn unwrap_strips_wrapline_flag_from_joined_cells() {
        let rows = vec![wrapped_row("ab"), line("c")];
        let logical = unwrap_rows(&rows);
        assert!(
            logical[0].cells.iter().all(|c| !c.flags.contains(Flags::WRAPLINE)),
            "WRAPLINE must not survive into a logical line"
        );
    }

    #[test]
    fn unwrap_trims_trailing_default_blanks_but_keeps_colored() {
        // "ab" followed by default padding spaces -> trimmed to "ab".
        let mut padded = line("ab");
        padded.cells.push(cell(' '));
        padded.cells.push(cell(' '));
        let logical = unwrap_rows(std::slice::from_ref(&padded));
        assert_eq!(logical[0].text_chars().collect::<String>(), "ab");

        // A trailing space with a non-default background is real content.
        let colored_bar = StyledLine {
            cells: vec![
                cell('a'),
                cell('b'),
                styled(' ', fg_default(), Color::Named(NamedColor::Red), Flags::empty()),
            ],
        };
        let logical = unwrap_rows(std::slice::from_ref(&colored_bar));
        assert_eq!(logical[0].cells.len(), 3, "colored trailing space is kept");
    }

    #[test]
    fn unwrap_drops_wide_char_spacers_keeps_wide_char() {
        let row = StyledLine {
            cells: vec![
                styled('日', fg_default(), bg_default(), Flags::WIDE_CHAR),
                styled(' ', fg_default(), bg_default(), Flags::WIDE_CHAR_SPACER),
                cell('x'),
            ],
        };
        let logical = unwrap_rows(std::slice::from_ref(&row));
        assert_eq!(logical[0].cells.len(), 2, "spacer cell is dropped");
        assert_eq!(logical[0].cells[0].c, '日');
        assert!(
            logical[0].cells[0].flags.contains(Flags::WIDE_CHAR),
            "the wide char keeps its WIDE_CHAR flag"
        );
        assert_eq!(logical[0].cells[1].c, 'x');
    }

    #[test]
    fn unwrap_then_encode_roundtrips() {
        // The two halves compose: un-wrapping grid rows then encoding
        // and decoding yields the same logical lines.
        let rows = vec![wrapped_row("hello "), line("world   "), line("next line")];
        let logical = unwrap_rows(&rows);
        let bytes = encode_chunk(&logical, true);
        assert_eq!(decode_chunk(&bytes).unwrap(), logical);
        assert_eq!(logical[0].text_chars().collect::<String>(), "hello world");
        assert_eq!(logical[1].text_chars().collect::<String>(), "next line");
    }
}
