#!/usr/bin/env bash
#
# deploy.sh — push site/src to S3 and invalidate the CloudFront cache.
# Run bootstrap.sh once first; it writes config.env that this reads.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="${HERE}/../src"
# shellcheck source=/dev/null
source "${HERE}/config.env"

say() { printf '\n\033[36m==>\033[0m %s\n' "$*"; }

# Long-cache the fingerprint-free assets (icons rarely change),
# but never cache index.html so edits show up after invalidation.
say "Syncing immutable assets to s3://${BUCKET}"
aws s3 sync "${SRC}/" "s3://${BUCKET}/" --delete \
  --exclude "index.html" \
  --cache-control "public, max-age=604800"

say "Uploading index.html (no-cache)"
aws s3 cp "${SRC}/index.html" "s3://${BUCKET}/index.html" \
  --cache-control "no-cache" \
  --content-type "text/html; charset=utf-8"

say "Invalidating CloudFront cache"
aws cloudfront create-invalidation --distribution-id "${DISTRIBUTION_ID}" --paths "/*" >/dev/null

say "Deployed. https://termica.io/  (CDN: ${DISTRIBUTION_DOMAIN})"
