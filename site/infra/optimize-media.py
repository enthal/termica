#!/usr/bin/env python3
"""Turn raw homepage captures into the small WebP variants the page serves.

Drop full-resolution (retina) PNG captures into ``site/media-src/``:

    site/media-src/hero.png         the 3-pane workspace shot
    site/media-src/history.png      the Ctrl+R overlay   (future)
    site/media-src/completion.png   the Tab popup        (future)

then run this script (no args). For each one it writes, into ``site/src/img/``:

    <name>.webp     1x, capped at DISPLAY_W px wide
    <name>@2x.webp  2x, capped at DISPLAY_W*2 px wide

and, from the hero, a 1200x630 ``og.png`` social card.

We ship WebP only — it's universally supported and roughly a third the size
of PNG for these screenshots. The page never references a PNG fallback, and
the raw originals are NOT committed (see site/media-src/.gitignore); they are
the source of truth on disk and can be re-captured. Missing inputs are
skipped, so you can drop them in one at a time.

Requires Pillow with WebP support.
"""

from pathlib import Path
import sys

from PIL import Image

HERE = Path(__file__).resolve().parent
SRC_DIR = HERE.parent / "media-src"
OUT_DIR = HERE.parent / "src" / "img"

# The hero renders at most ~960px wide (.shot max-width: 60rem). 1000px gives a
# little headroom for 1x; @2x is double that for retina. Anything larger is
# bytes the browser throws away.
DISPLAY_W = 1000

# name -> whether to also emit the OG card from it
TARGETS = {"hero": True, "history": False, "completion": False}

WEBP_QUALITY = 80
OG_SIZE = (1200, 630)


def _fit_width(img: Image.Image, target_w: int) -> Image.Image:
    """Downscale to target_w wide (never upscale)."""
    w, h = img.size
    if w <= target_w:
        return img
    return img.resize((target_w, round(h * target_w / w)), Image.LANCZOS)


def emit(name: str, make_og: bool) -> bool:
    src = SRC_DIR / f"{name}.png"
    if not src.exists():
        print(f"  · skip {name}: no {src.relative_to(HERE.parent.parent)}")
        return False

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    img = Image.open(src).convert("RGB")

    one = _fit_width(img, DISPLAY_W)
    two = _fit_width(img, DISPLAY_W * 2)
    one.save(OUT_DIR / f"{name}.webp", quality=WEBP_QUALITY, method=6)
    two.save(OUT_DIR / f"{name}@2x.webp", quality=WEBP_QUALITY, method=6)
    print(f"  ✓ {name}: 1x {one.size[0]}x{one.size[1]} + 2x {two.size[0]}x{two.size[1]} (webp)")

    if make_og:
        og = _cover_crop(img, OG_SIZE)
        og.save(OUT_DIR / "og.png", optimize=True)
        print(f"  ✓ og.png: {OG_SIZE[0]}x{OG_SIZE[1]} (from {name})")
    return True


def _cover_crop(img: Image.Image, size: tuple[int, int]) -> Image.Image:
    """Scale to cover the target box, then center-crop to it."""
    tw, th = size
    sw, sh = img.size
    scale = max(tw / sw, th / sh)
    rw, rh = round(sw * scale), round(sh * scale)
    img = img.resize((rw, rh), Image.LANCZOS)
    left, top = (rw - tw) // 2, (rh - th) // 2
    return img.crop((left, top, left + tw, top + th))


def main() -> int:
    print(f"media-src: {SRC_DIR}")
    any_done = False
    for name, make_og in TARGETS.items():
        any_done |= emit(name, make_og)
    if not any_done:
        print("Nothing to do — drop captures into site/media-src/ first.")
        return 1
    print("Done. Review site/src/img/, then run ./infra/deploy.sh")
    return 0


if __name__ == "__main__":
    sys.exit(main())
