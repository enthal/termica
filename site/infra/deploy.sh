#!/usr/bin/env bash
#
# deploy.sh — build site/dist (content-hashed assets) and push it to S3,
# then invalidate the CloudFront cache. Run bootstrap.sh once first; it
# writes config.env that this reads.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST="${HERE}/../dist"
# shellcheck source=/dev/null
source "${HERE}/config.env"

say() { printf '\n\033[36m==>\033[0m %s\n' "$*"; }

# 1. Build dist/ with fingerprinted filenames.
bash "${HERE}/build.sh"

# 2. Fingerprinted assets are immutable — their name changes when their
#    content does, so they can be cached forever.
say "Syncing fingerprinted assets to s3://${BUCKET}"
aws s3 sync "${DIST}/" "s3://${BUCKET}/" --delete \
  --exclude "index.html" \
  --cache-control "public, max-age=31536000, immutable"

# 3. Favicons keep their conventional (unhashed) names, so cap their cache
#    rather than pinning them forever.
say "Re-caching favicons (short TTL)"
for f in favicon.ico favicon-16.png favicon-32.png apple-touch-icon.png; do
  [ -f "${DIST}/${f}" ] && aws s3 cp "${DIST}/${f}" "s3://${BUCKET}/${f}" \
    --cache-control "public, max-age=3600" >/dev/null
done

# 4. index.html is never cached, so a new build's hashes take effect at once.
say "Uploading index.html (no-cache)"
aws s3 cp "${DIST}/index.html" "s3://${BUCKET}/index.html" \
  --cache-control "no-cache" \
  --content-type "text/html; charset=utf-8" >/dev/null

say "Invalidating CloudFront cache"
aws cloudfront create-invalidation --distribution-id "${DISTRIBUTION_ID}" --paths "/*" >/dev/null

say "Deployed. https://termica.io/  (CDN: ${DISTRIBUTION_DOMAIN})"
