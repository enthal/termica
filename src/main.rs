// Termica binary entry point.
//
// All application logic lives in the library crate (`src/lib.rs`)
// so tests can drive it without going through `main`. See SPEC.md
// and CLAUDE.md.

#![forbid(unsafe_code)]

fn main() -> eframe::Result<()> {
    termica::run()
}
