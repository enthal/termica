//! Variants for the visual separation between sealed command blocks
//! in the live pane.
//!
//! Today blocks butt up against each other with only egui's
//! `item_spacing` between them, so a stack of short `ls`-like
//! commands runs together.
//!
//! Run:   cargo run --example pick_block_separator
//!
//! Picks by writing the variant id to
//! `/tmp/termica-picker-choice.txt`.

#![forbid(unsafe_code)]

use eframe::egui;
use termica::visual_picker::{Variant, run};

const BLOCKS: &[(&str, &str, Option<i32>)] = &[
    ("ls", "Cargo.toml  README.md  src  spec  tests", Some(0)),
    ("git status", "On branch feat/history-6\nUntracked files:\n  profile.json.gz", Some(0)),
    ("cargo check", "error: cannot find macro `foo` in this scope", Some(1)),
    ("pwd", "/Users/tim/git/enthal/termica", Some(0)),
];

const BLOCK_W: f32 = 460.0;

fn paint_block(ui: &mut egui::Ui, cmd: &str, out: &str, exit: Option<i32>) {
    let mono = egui::FontId::monospace(13.0);
    let failed = matches!(exit, Some(n) if n != 0);
    // Failed-block bg slot (matches production behaviour).
    let bg_idx = if failed { Some(ui.painter().add(egui::Shape::Noop)) } else { None };
    // Chip row.
    ui.horizontal(|ui| {
        let chip_color = egui::Color32::from_rgb(0x22, 0x22, 0x22);
        let chip_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0x44, 0x44, 0x44));
        // cwd chip (faked).
        for label in ["~/git/enthal/termica", "exit 1"]
            .iter()
            .enumerate()
            .filter_map(|(i, l)| if i == 1 && !failed { None } else { Some(*l) })
        {
            let galley = ui.fonts_mut(|f| {
                f.layout_no_wrap(
                    label.to_string(),
                    mono.clone(),
                    egui::Color32::from_rgb(0xa0, 0xa0, 0xa0),
                )
            });
            let pad_x = 8.0;
            let pad_y = 3.0;
            let chip_w = galley.size().x + 2.0 * pad_x;
            let chip_h = galley.size().y + 2.0 * pad_y;
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(chip_w, chip_h), egui::Sense::hover());
            ui.painter().rect_filled(rect, 4.0, chip_color);
            ui.painter().rect_stroke(rect, 4.0, chip_stroke, egui::StrokeKind::Inside);
            ui.painter().galley(rect.min + egui::vec2(pad_x, pad_y), galley, egui::Color32::WHITE);
        }
    });
    // Command label.
    ui.label(egui::RichText::new(cmd).monospace().color(egui::Color32::from_rgb(0x6e, 0xd0, 0xe8)));
    // Output.
    for line in out.split('\n') {
        ui.label(
            egui::RichText::new(line).monospace().color(egui::Color32::from_rgb(0xd8, 0xd8, 0xd8)),
        );
    }
    if let Some(idx) = bg_idx {
        // Approximate block rect for the wash. The picker is a rough
        // sample — exact pixels aren't load-bearing here.
        let r = ui.min_rect();
        ui.painter().set(
            idx,
            egui::Shape::rect_filled(
                r.expand2(egui::vec2(4.0, 2.0)),
                4.0,
                egui::Color32::from_rgba_premultiplied(0x20, 0x08, 0x08, 0x40),
            ),
        );
    }
}

fn paint_blocks_with_separator(
    ui: &mut egui::Ui,
    extra_gap: f32,
    hairline: Option<u8>, // alpha for hairline color, 0 means none
) {
    let n = BLOCKS.len();
    for (i, (cmd, out, exit)) in BLOCKS.iter().enumerate() {
        ui.scope(|ui| {
            ui.set_width(BLOCK_W);
            paint_block(ui, cmd, out, *exit);
        });
        if i + 1 < n {
            if extra_gap > 0.0 {
                ui.add_space(extra_gap);
            }
            if let Some(alpha) = hairline
                && alpha > 0
            {
                {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(BLOCK_W, 1.0), egui::Sense::hover());
                    ui.painter().line_segment(
                        [rect.left_center(), rect.right_center()],
                        egui::Stroke::new(
                            1.0,
                            egui::Color32::from_rgba_premultiplied(0xa0, 0xa0, 0xa0, alpha),
                        ),
                    );
                    ui.add_space(extra_gap);
                }
            }
        }
    }
}

fn paint_natural(ui: &mut egui::Ui) {
    paint_blocks_with_separator(ui, 0.0, None);
}
fn paint_gap_only_small(ui: &mut egui::Ui) {
    paint_blocks_with_separator(ui, 8.0, None);
}
fn paint_gap_only_large(ui: &mut egui::Ui) {
    paint_blocks_with_separator(ui, 16.0, None);
}
fn paint_dim_hairline(ui: &mut egui::Ui) {
    paint_blocks_with_separator(ui, 4.0, Some(0x40));
}
fn paint_dimmer_hairline_more_gap(ui: &mut egui::Ui) {
    paint_blocks_with_separator(ui, 8.0, Some(0x30));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::path::PathBuf::from("/tmp/termica-picker-choice.txt");
    let _ = std::fs::remove_file(&output);
    let choice = run(
        "Sealed-block separator (gap and/or hairline)",
        vec![
            Variant::new("natural", "A · No extra gap (today)", paint_natural),
            Variant::new("gap-8", "B · 8px gap, no rule", paint_gap_only_small),
            Variant::new("gap-16", "C · 16px gap, no rule", paint_gap_only_large),
            Variant::new(
                "hairline-4-40",
                "D · 4px + dim hairline (alpha 0x40)",
                paint_dim_hairline,
            ),
            Variant::new(
                "hairline-8-30",
                "E · 8px + dimmer hairline (alpha 0x30)",
                paint_dimmer_hairline_more_gap,
            ),
        ],
        &output,
    )?;
    match choice {
        Some(id) => println!("picked: {id} (written to {})", output.display()),
        None => println!("cancelled (no file written)"),
    }
    Ok(())
}
