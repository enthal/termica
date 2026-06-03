//! Faint app-icon watermark painted behind a *blank* pane (one with
//! no sealed scrollback blocks yet), plus the live-tunable settings
//! the `--pick-watermark` picker window drives.
//!
//! The watermark is an **overlay**, not a true background: the
//! terminal paints an opaque full-pane fill ([`crate::render`]), so
//! anything drawn under it is invisible. We instead paint the icon on
//! top at low alpha — over a fresh prompt there is almost nothing to
//! occlude, so it reads as a watermark while the prompt text shows
//! through faintly.
//!
//! Tuning the alpha / size by eye is the whole point of the picker
//! viewport, so the appearance lives in a single [`WatermarkSettings`]
//! value shared via `Arc<Mutex<…>>` (mirrors the chrome picker).

use eframe::egui;

/// Live-tunable watermark appearance. Small and `Copy`, so the app
/// snapshots it out of its `Arc<Mutex<…>>` once per frame into
/// `TabBehavior` exactly like the focused-chrome variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatermarkSettings {
    /// Master on/off. When false the overlay is never painted.
    pub enabled: bool,
    /// Opacity, `0..=255`, applied as the painter tint's alpha. A
    /// white tint at this alpha renders the icon at `alpha/255`
    /// opacity with its colors intact.
    pub alpha: u8,
    /// Square edge length as a fraction of the pane's shorter side.
    pub size_frac: f32,
    /// Render the icon desaturated. Often reads better as a watermark
    /// than full color; backed by a separately-cached gray texture.
    pub grayscale: bool,
}

impl Default for WatermarkSettings {
    fn default() -> Self {
        // Subtle by default: a quarter-opacity logo at half the pane's
        // short side. Tuned further live via `--pick-watermark`.
        Self { enabled: true, alpha: 28, size_frac: 0.5, grayscale: false }
    }
}

/// Smallest / largest size fractions the picker slider offers. A
/// zero-size watermark is pointless and a >1.0 one overflows the
/// pane; clamped here so neither the slider nor a bad persisted value
/// can produce a degenerate rect.
pub const MIN_SIZE_FRAC: f32 = 0.05;
pub const MAX_SIZE_FRAC: f32 = 1.0;

/// Centered square destination rect for the watermark within `pane`.
///
/// Edge = `size_frac` (clamped to `[MIN_SIZE_FRAC, MAX_SIZE_FRAC]`)
/// times the pane's *shorter* side, so the square always fits even in
/// a tall-thin or wide-short pane. Pure → unit-tested below.
pub fn watermark_rect(pane: egui::Rect, size_frac: f32) -> egui::Rect {
    let frac = size_frac.clamp(MIN_SIZE_FRAC, MAX_SIZE_FRAC);
    let edge = pane.size().min_elem() * frac;
    egui::Rect::from_center_size(pane.center(), egui::vec2(edge, edge))
}

/// Full-image UV rect (`0,0 → 1,1`) for `Painter::image`.
fn full_uv() -> egui::Rect {
    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
}

/// Decode the embedded app icon into an egui `ColorImage`. `grayscale`
/// maps each pixel to its luma while preserving the original alpha, so
/// transparent corners stay transparent. Returns `None` if the bytes
/// fail to decode — a missing watermark is purely cosmetic.
fn decode_icon(grayscale: bool) -> Option<egui::ColorImage> {
    let rgba = image::load_from_memory_with_format(crate::APP_ICON_PNG, image::ImageFormat::Png)
        .ok()?
        .to_rgba8();
    let (w, h) = rgba.dimensions();
    let size = [w as usize, h as usize];
    if !grayscale {
        return Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()));
    }
    // Rec. 601 luma, alpha untouched.
    let mut out = Vec::with_capacity(rgba.as_raw().len());
    for px in rgba.pixels() {
        let [r, g, b, a] = px.0;
        let y = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32).round() as u8;
        out.extend_from_slice(&[y, y, y, a]);
    }
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &out))
}

/// Upload-once, cached texture handle for the watermark. Memoized in
/// egui temp memory (keyed by the grayscale flag) so we decode +
/// upload a given variant exactly once and share it across every pane
/// and frame; `TextureHandle` is `Arc`-backed, so the cached clone is
/// cheap and keeps the texture alive.
fn texture(ctx: &egui::Context, grayscale: bool) -> Option<egui::TextureHandle> {
    let id = egui::Id::new(("termica-watermark-texture", grayscale));
    if let Some(handle) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return Some(handle);
    }
    let image = decode_icon(grayscale)?;
    let handle = ctx.load_texture(
        if grayscale { "termica-watermark-gray" } else { "termica-watermark" },
        image,
        egui::TextureOptions::LINEAR,
    );
    ctx.data_mut(|d| d.insert_temp(id, handle.clone()));
    Some(handle)
}

/// Paint the watermark overlay into `pane_rect` per `settings`.
///
/// Callers gate on "is this pane blank?" (see
/// [`crate::block::BlockStack::has_sealed_blocks`]) and on alt-screen
/// mode; this function only honors `settings.enabled` and the texture
/// being decodable. Paints in the caller's current layer/paint order,
/// so call it *after* the terminal so the faint logo lands on top.
pub fn paint(
    ctx: &egui::Context,
    painter: &egui::Painter,
    pane_rect: egui::Rect,
    settings: WatermarkSettings,
) {
    if !settings.enabled || settings.alpha == 0 {
        return;
    }
    let Some(tex) = texture(ctx, settings.grayscale) else {
        return;
    };
    let rect = watermark_rect(pane_rect, settings.size_frac);
    let tint = egui::Color32::from_white_alpha(settings.alpha);
    painter.image(tex.id(), rect, full_uv(), tint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Rect, pos2};

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(10.0, 20.0), egui::vec2(w, h))
    }

    #[test]
    fn watermark_rect_is_centered_and_square() {
        let pane = rect(400.0, 200.0);
        let wm = watermark_rect(pane, 0.5);
        // Square: edge = 0.5 * min(400,200) = 100.
        assert_eq!(wm.width(), 100.0);
        assert_eq!(wm.height(), 100.0);
        // Centered on the pane.
        assert_eq!(wm.center(), pane.center());
    }

    #[test]
    fn watermark_rect_uses_shorter_side() {
        // Tall-thin pane: the square keys off the width (the shorter
        // side) so it never overflows horizontally.
        let pane = rect(120.0, 900.0);
        let wm = watermark_rect(pane, 1.0);
        assert_eq!(wm.width(), 120.0);
        assert_eq!(wm.height(), 120.0);
        assert!(pane.contains_rect(wm));
    }

    #[test]
    fn watermark_rect_clamps_size_frac() {
        let pane = rect(200.0, 200.0);
        // Below MIN clamps up.
        let small = watermark_rect(pane, 0.0);
        assert_eq!(small.width(), 200.0 * MIN_SIZE_FRAC);
        // Above MAX clamps down.
        let big = watermark_rect(pane, 5.0);
        assert_eq!(big.width(), 200.0 * MAX_SIZE_FRAC);
    }

    #[test]
    fn embedded_icon_decodes_both_variants() {
        // No GPU/context needed to decode; just proves the asset is a
        // valid PNG and the grayscale path produces a same-sized image.
        let color = decode_icon(false).expect("color decode");
        let gray = decode_icon(true).expect("gray decode");
        assert_eq!(color.size, gray.size);
        assert!(color.size[0] > 0 && color.size[1] > 0);
    }
}
