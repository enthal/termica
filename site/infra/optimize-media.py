#!/usr/bin/env python3
"""Turn raw homepage captures into the web variants the page references.

Drop full-resolution (2x / retina) PNG captures into ``site/media-src/``:

    site/media-src/hero.png         the 3-pane workspace shot
    site/media-src/history.png      the Ctrl+R overlay
    site/media-src/completion.png   the Tab popup

then run this script (no args). For each one it writes, into ``site/src/img/``:

    <name>@2x.png   optimized full-res          <name>@2x.webp
    <name>.png      half-size (1x)              <name>.webp

and, from the hero, a 1200x630 ``og.png`` social card. Missing inputs are
skipped with a warning, so you can drop them in one at a time.

Requires Pillow with WebP support (already present on this machine).
"""

from pathlib import Path
import sys

from PIL import Image

HERE = Path(__file__).resolve().parent
SRC_DIR = HERE.parent / "media-src"
OUT_DIR = HERE.parent / "src" / "img"

# name -> whether to also emit the OG card from it
TARGETS = {"hero": True, "history": False, "completion": False}

WEBP_QUALITY = 82
OG_SIZE = (1200, 630)


def emit(name: str, make_og: bool) -> bool:
    src = SRC_DIR / f"{name}.png"
    if not src.exists():
        print(f"  · skip {name}: no {src.relative_to(HERE.parent.parent)}")
        return False

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    img = Image.open(src).convert("RGBA")
    w, h = img.size

    # 2x = the source as-is; 1x = half (rounded), never upscaled.
    one = img.resize((max(1, w // 2), max(1, h // 2)), Image.LANCZOS)

    img.save(OUT_DIR / f"{name}@2x.png", optimize=True)
    one.save(OUT_DIR / f"{name}.png", optimize=True)
    img.save(OUT_DIR / f"{name}@2x.webp", quality=WEBP_QUALITY, method=6)
    one.save(OUT_DIR / f"{name}.webp", quality=WEBP_QUALITY, method=6)
    print(f"  ✓ {name}: {w}x{h} -> @2x + 1x ({w//2}x{h//2}), png + webp")

    if make_og:
        og = _cover_crop(img.convert("RGB"), OG_SIZE)
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
