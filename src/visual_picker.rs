//! A tiny eframe app for visual A/B/C/D decisions.
//!
//! Usage from an `examples/` binary:
//!
//! ```ignore
//! use termica::visual_picker::{run, Variant};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     run(
//!         "row separator style",
//!         vec![
//!             Variant::new("more-spacing", "More vertical space", paint_more_spacing),
//!             Variant::new("hairline",     "Faint hairline rule", paint_hairline),
//!         ],
//!         std::path::Path::new("/tmp/termica-picker-choice.txt"),
//!     )
//! }
//! ```
//!
//! The window shows each variant in its own card with a "Pick this"
//! button. Clicking the button writes the variant's stable id (the
//! first arg to [`Variant::new`]) to `output_path` and closes the
//! window. The caller (an agent or human) then reads that file to
//! find out which option was chosen.
//!
//! Closing the window without picking writes nothing — the file is
//! either absent or stale from a previous run; treat absence as
//! "user cancelled".

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eframe::egui;

/// One option presented in the picker window. The `painter` is a
/// pure egui callback that renders the variant inside a card; it
/// gets a fresh [`egui::Ui`] sized to a fraction of the window.
#[derive(Clone)]
pub struct Variant {
    /// Stable identifier written to the output file when the user
    /// picks this variant. Use a short kebab-case slug — it's what
    /// the caller pattern-matches on.
    pub id: &'static str,
    /// Human-readable label shown above the card.
    pub label: &'static str,
    /// Renders the variant. Called once per frame.
    pub painter: Arc<dyn Fn(&mut egui::Ui) + Send + Sync>,
}

impl Variant {
    pub fn new(
        id: &'static str,
        label: &'static str,
        painter: impl Fn(&mut egui::Ui) + Send + Sync + 'static,
    ) -> Self {
        Self { id, label, painter: Arc::new(painter) }
    }
}

impl std::fmt::Debug for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Variant").field("id", &self.id).field("label", &self.label).finish()
    }
}

/// Block on a picker window. Returns `Ok(Some(id))` when the user
/// clicked one, `Ok(None)` when they closed without picking, and
/// `Err(_)` if eframe failed to launch.
pub fn run(
    title: &str,
    variants: Vec<Variant>,
    output_path: &Path,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let chosen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let app_title = format!("Termica visual picker · {title}");
    let title_for_app = title.to_string();
    let chosen_for_app = chosen.clone();
    let output_path_for_app = output_path.to_path_buf();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        &app_title,
        options,
        Box::new(move |_cc| {
            Ok(Box::new(PickerApp {
                title: title_for_app,
                variants,
                output_path: output_path_for_app,
                chosen: chosen_for_app,
            }))
        }),
    )?;
    let final_choice = chosen.lock().expect("picker mutex").clone();
    Ok(final_choice)
}

struct PickerApp {
    title: String,
    variants: Vec<Variant>,
    output_path: PathBuf,
    chosen: Arc<Mutex<Option<String>>>,
}

impl eframe::App for PickerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("Pick a variant: {}", self.title));
            ui.weak("Click 'Pick this' under your preferred option.");
            ui.separator();
            // 2-column grid. For ≤ 2 variants, single row; for 3-4,
            // 2×2; for 5+, scroll.
            let cols = if self.variants.len() <= 1 { 1 } else { 2 };
            let mut picked: Option<&'static str> = None;
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("picker-grid").num_columns(cols).spacing([16.0, 16.0]).show(
                    ui,
                    |ui| {
                        for (i, variant) in self.variants.iter().enumerate() {
                            let card_response = egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_width(440.0);
                                ui.vertical(|ui| {
                                    ui.heading(variant.label);
                                    ui.add_space(6.0);
                                    (variant.painter)(ui);
                                    ui.add_space(8.0);
                                    if ui.button("Pick this").clicked() {
                                        picked = Some(variant.id);
                                    }
                                });
                            });
                            let _ = card_response;
                            if (i + 1) % cols == 0 {
                                ui.end_row();
                            }
                        }
                    },
                );
            });
            if let Some(id) = picked {
                // Write the choice + close. Use a temp + rename
                // so a reader that polls never sees a truncated
                // file. Errors are surfaced via stderr; we still
                // close so the user isn't stuck.
                let tmp = self.output_path.with_extension("partial");
                if let Err(e) = std::fs::write(&tmp, id) {
                    eprintln!("picker: write {} failed: {e}", tmp.display());
                } else if let Err(e) = std::fs::rename(&tmp, &self.output_path) {
                    eprintln!("picker: rename to {} failed: {e}", self.output_path.display());
                }
                *self.chosen.lock().expect("picker mutex") = Some(id.to_string());
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}
