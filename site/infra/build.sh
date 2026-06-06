#!/usr/bin/env bash
#
# build.sh — produce site/dist/ from site/src/ with content-hashed asset
# filenames. The no-cache index.html then points at fingerprinted assets
# (e.g. style.4f3a9c21.css) that can be cached forever — when content
# changes the filename changes, so a browser can never serve a stale copy.
#
# src/ stays the editable source (and is what you preview locally with an
# unhashed plain http server); dist/ is the generated deploy artifact.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="${HERE}/../src"
DIST="${HERE}/../dist"
INDEX="${DIST}/index.html"

# Assets referenced from index.html that change with content and must be
# fingerprinted. favicon.ico is intentionally NOT here: browsers request
# /favicon.ico by convention, so it keeps its fixed name (deploy.sh gives
# it a short cache instead).
FINGERPRINT=(
  style.css
  icon.png
  img/hero.webp
  img/hero@2x.webp
  img/og.jpg
)

say() { printf '\n\033[36m==>\033[0m %s\n' "$*"; }

say "Building ${DIST#"${HERE}/../"} from ${SRC#"${HERE}/../"}"
rm -rf "${DIST}"
cp -R "${SRC}" "${DIST}"

for rel in "${FINGERPRINT[@]}"; do
  f="${DIST}/${rel}"
  if [ ! -f "$f" ]; then
    echo "  · skip ${rel} (not present)"
    continue
  fi

  hash="$(shasum -a 256 "$f" | cut -c1-8)"
  dir="$(dirname "$rel")"
  base="$(basename "$rel")"
  stem="${base%.*}"
  ext="${base##*.}"
  hashed="${stem}.${hash}.${ext}"
  newrel="${hashed}"
  [ "$dir" = "." ] || newrel="${dir}/${hashed}"

  mv "$f" "${DIST}/${newrel}"
  # Rewrite every "/<rel>" reference (covers the absolute og:image URL too,
  # since it contains the path). The leading slash keeps icon.png from
  # matching inside apple-touch-icon.png etc.
  sed "s|/${rel}|/${newrel}|g" "${INDEX}" > "${INDEX}.tmp" && mv "${INDEX}.tmp" "${INDEX}"
  echo "  ✓ ${rel} -> ${newrel}"
done

say "Built. Deploy with ./infra/deploy.sh"
