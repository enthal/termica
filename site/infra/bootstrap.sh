#!/usr/bin/env bash
#
# bootstrap.sh — one-time AWS setup for the termica.io static homepage.
#
# Creates, in us-east-1:
#   * a PRIVATE S3 bucket (no public access; reached only via CloudFront OAC)
#   * an ACM certificate for termica.io + www.termica.io (DNS-validated via Route53)
#   * a CloudFront Origin Access Control (OAC)
#   * a CloudFront distribution (HTTPS, HTTP->HTTPS redirect, both aliases)
#   * a CloudFront Function that 301-redirects www.termica.io -> termica.io
#   * Route53 A + AAAA alias records for the apex and www -> the distribution
#   * an S3 bucket policy granting ONLY this distribution read access
#
# Idempotent-ish: re-running reuses existing resources where it can. It is safe
# to re-run if it dies partway through.
#
# Requires: aws CLI v2, jq. The credentials in your environment must have rights
# over S3, ACM, CloudFront, and Route53.
#
# After this finishes, run ./deploy.sh to push site/src to the bucket.

set -euo pipefail

# ---- configuration ----------------------------------------------------------
DOMAIN="termica.io"
WWW="www.${DOMAIN}"
BUCKET="termica.io"                 # bucket name; private, name need not match domain
REGION="us-east-1"                  # CloudFront certs MUST be in us-east-1
CALLER_REF="termica-io-$(date +%s)" # unique per CloudFront create call
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STATE="${HERE}/config.env"          # written for deploy.sh to consume
CF_HOSTED_ZONE_ID="Z2FDTNDATAQYW2"  # fixed CloudFront alias target zone (global)

say() { printf '\n\033[36m==>\033[0m %s\n' "$*"; }

command -v aws >/dev/null || { echo "aws CLI not found"; exit 1; }
command -v jq  >/dev/null || { echo "jq not found"; exit 1; }

ACCOUNT_ID="$(aws sts get-caller-identity --query Account --output text)"
say "AWS account: ${ACCOUNT_ID}"

# ---- Route53 hosted zone ----------------------------------------------------
say "Looking up Route53 hosted zone for ${DOMAIN}"
HOSTED_ZONE_ID="$(aws route53 list-hosted-zones-by-name \
  --dns-name "${DOMAIN}." \
  --query "HostedZones[?Name=='${DOMAIN}.'].Id | [0]" --output text | sed 's#/hostedzone/##')"
[ -n "${HOSTED_ZONE_ID}" ] && [ "${HOSTED_ZONE_ID}" != "None" ] \
  || { echo "No Route53 hosted zone for ${DOMAIN}. Create it (or finish the registrar transfer) first."; exit 1; }
say "Hosted zone: ${HOSTED_ZONE_ID}"

# ---- S3 bucket (private) ----------------------------------------------------
if aws s3api head-bucket --bucket "${BUCKET}" 2>/dev/null; then
  say "Bucket s3://${BUCKET} already exists"
else
  say "Creating private bucket s3://${BUCKET} in ${REGION}"
  # us-east-1 must NOT pass a LocationConstraint.
  aws s3api create-bucket --bucket "${BUCKET}" --region "${REGION}"
fi

say "Locking the bucket down (block all public access, enforce ownership)"
aws s3api put-public-access-block --bucket "${BUCKET}" \
  --public-access-block-configuration \
  BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true
aws s3api put-bucket-ownership-controls --bucket "${BUCKET}" \
  --ownership-controls 'Rules=[{ObjectOwnership=BucketOwnerEnforced}]'

# ---- ACM certificate (DNS-validated) ----------------------------------------
say "Finding or requesting an ACM cert for ${DOMAIN} (+${WWW})"
CERT_ARN="$(aws acm list-certificates --region "${REGION}" \
  --query "CertificateSummaryList[?DomainName=='${DOMAIN}'].CertificateArn | [0]" --output text)"

if [ -z "${CERT_ARN}" ] || [ "${CERT_ARN}" = "None" ]; then
  CERT_ARN="$(aws acm request-certificate --region "${REGION}" \
    --domain-name "${DOMAIN}" \
    --subject-alternative-names "${WWW}" \
    --validation-method DNS \
    --query CertificateArn --output text)"
  say "Requested certificate: ${CERT_ARN}"
  sleep 5  # let the validation records populate
fi
say "Certificate: ${CERT_ARN}"

say "Writing DNS validation records into Route53"
# Collect the (possibly two) unique CNAME validation records and upsert them.
VALIDATION="$(aws acm describe-certificate --region "${REGION}" --certificate-arn "${CERT_ARN}" \
  --query "Certificate.DomainValidationOptions[].ResourceRecord" --output json)"
echo "${VALIDATION}" | jq -c 'unique_by(.Name)[]' | while read -r rec; do
  NAME="$(echo "${rec}" | jq -r .Name)"
  VALUE="$(echo "${rec}" | jq -r .Value)"
  [ -z "${NAME}" ] && continue
  aws route53 change-resource-record-sets --hosted-zone-id "${HOSTED_ZONE_ID}" \
    --change-batch "$(jq -n --arg n "${NAME}" --arg v "${VALUE}" '{
      Changes:[{Action:"UPSERT",ResourceRecordSet:{
        Name:$n,Type:"CNAME",TTL:300,ResourceRecords:[{Value:$v}]}}]}')" >/dev/null
done

say "Waiting for certificate validation (a few minutes)..."
aws acm wait certificate-validated --region "${REGION}" --certificate-arn "${CERT_ARN}"
say "Certificate validated."

# ---- CloudFront Function: www -> apex 301 -----------------------------------
say "Creating/locating CloudFront Function for www -> apex redirect"
FUNC_NAME="termica-www-to-apex"
FUNC_CODE_FILE="$(mktemp)"
cat > "${FUNC_CODE_FILE}" <<'JS'
function handler(event) {
  var req = event.request;
  var host = req.headers.host && req.headers.host.value;
  if (host === 'www.termica.io') {
    return {
      statusCode: 301,
      statusDescription: 'Moved Permanently',
      headers: { location: { value: 'https://termica.io' + req.uri } }
    };
  }
  return req;
}
JS

FUNC_ARN="$(aws cloudfront list-functions \
  --query "FunctionList.Items[?Name=='${FUNC_NAME}'].FunctionMetadata.FunctionARN | [0]" --output text 2>/dev/null || true)"
if [ -z "${FUNC_ARN}" ] || [ "${FUNC_ARN}" = "None" ]; then
  CREATE="$(aws cloudfront create-function \
    --name "${FUNC_NAME}" \
    --function-config Comment="Redirect www.termica.io to apex",Runtime="cloudfront-js-2.0" \
    --function-code "fileb://${FUNC_CODE_FILE}")"
  FUNC_ARN="$(echo "${CREATE}" | jq -r .FunctionSummary.FunctionMetadata.FunctionARN)"
  ETAG="$(echo "${CREATE}" | jq -r .ETag)"
  aws cloudfront publish-function --name "${FUNC_NAME}" --if-match "${ETAG}" >/dev/null
  say "Published function: ${FUNC_ARN}"
else
  say "Reusing function: ${FUNC_ARN}"
fi

# ---- CloudFront Origin Access Control ---------------------------------------
say "Creating/locating Origin Access Control"
OAC_NAME="termica-io-oac"
OAC_ID="$(aws cloudfront list-origin-access-controls \
  --query "OriginAccessControlList.Items[?Name=='${OAC_NAME}'].Id | [0]" --output text 2>/dev/null || true)"
if [ -z "${OAC_ID}" ] || [ "${OAC_ID}" = "None" ]; then
  OAC_ID="$(aws cloudfront create-origin-access-control \
    --origin-access-control-config \
    Name="${OAC_NAME}",SigningProtocol="sigv4",SigningBehavior="always",OriginAccessControlOriginType="s3" \
    --query OriginAccessControl.Id --output text)"
fi
say "OAC: ${OAC_ID}"

# ---- CloudFront distribution ------------------------------------------------
S3_DOMAIN="${BUCKET}.s3.${REGION}.amazonaws.com"
DIST_ID="$(aws cloudfront list-distributions \
  --query "DistributionList.Items[?Aliases.Items && contains(Aliases.Items, '${DOMAIN}')].Id | [0]" \
  --output text 2>/dev/null || true)"

if [ -z "${DIST_ID}" ] || [ "${DIST_ID}" = "None" ]; then
  say "Creating CloudFront distribution"
  DIST_CONFIG="$(mktemp)"
  cat > "${DIST_CONFIG}" <<JSON
{
  "CallerReference": "${CALLER_REF}",
  "Aliases": { "Quantity": 2, "Items": ["${DOMAIN}", "${WWW}"] },
  "DefaultRootObject": "index.html",
  "Origins": {
    "Quantity": 1,
    "Items": [{
      "Id": "s3-${BUCKET}",
      "DomainName": "${S3_DOMAIN}",
      "OriginAccessControlId": "${OAC_ID}",
      "S3OriginConfig": { "OriginAccessIdentity": "" }
    }]
  },
  "DefaultCacheBehavior": {
    "TargetOriginId": "s3-${BUCKET}",
    "ViewerProtocolPolicy": "redirect-to-https",
    "Compress": true,
    "CachePolicyId": "658327ea-f89d-4fab-a63d-7e88639e58f6",
    "FunctionAssociations": {
      "Quantity": 1,
      "Items": [{ "EventType": "viewer-request", "FunctionARN": "${FUNC_ARN}" }]
    }
  },
  "CustomErrorResponses": {
    "Quantity": 2,
    "Items": [
      { "ErrorCode": 403, "ResponseCode": "404", "ResponsePagePath": "/index.html", "ErrorCachingMinTTL": 60 },
      { "ErrorCode": 404, "ResponseCode": "404", "ResponsePagePath": "/index.html", "ErrorCachingMinTTL": 60 }
    ]
  },
  "Comment": "termica.io homepage",
  "Enabled": true,
  "HttpVersion": "http2and3",
  "ViewerCertificate": {
    "ACMCertificateArn": "${CERT_ARN}",
    "SSLSupportMethod": "sni-only",
    "MinimumProtocolVersion": "TLSv1.2_2021"
  }
}
JSON
  CREATE_DIST="$(aws cloudfront create-distribution --distribution-config "file://${DIST_CONFIG}")"
  DIST_ID="$(echo "${CREATE_DIST}" | jq -r .Distribution.Id)"
fi
DIST_DOMAIN="$(aws cloudfront get-distribution --id "${DIST_ID}" --query Distribution.DomainName --output text)"
DIST_ARN="arn:aws:cloudfront::${ACCOUNT_ID}:distribution/${DIST_ID}"
say "Distribution: ${DIST_ID} (${DIST_DOMAIN})"

# ---- S3 bucket policy: allow ONLY this distribution -------------------------
say "Granting the distribution read access to the bucket"
aws s3api put-bucket-policy --bucket "${BUCKET}" --policy "$(jq -n \
  --arg bucket "${BUCKET}" --arg arn "${DIST_ARN}" '{
  Version:"2012-10-17",
  Statement:[{
    Sid:"AllowCloudFrontServicePrincipalReadOnly",
    Effect:"Allow",
    Principal:{Service:"cloudfront.amazonaws.com"},
    Action:"s3:GetObject",
    Resource:("arn:aws:s3:::"+$bucket+"/*"),
    Condition:{StringEquals:{"AWS:SourceArn":$arn}}
  }]}')"

# ---- Route53 alias records (apex + www) -------------------------------------
say "Pointing ${DOMAIN} and ${WWW} at the distribution"
for HOST in "${DOMAIN}" "${WWW}"; do
  for TYPE in A AAAA; do
    aws route53 change-resource-record-sets --hosted-zone-id "${HOSTED_ZONE_ID}" \
      --change-batch "$(jq -n --arg h "${HOST}." --arg t "${TYPE}" \
        --arg dn "${DIST_DOMAIN}" --arg z "${CF_HOSTED_ZONE_ID}" '{
        Changes:[{Action:"UPSERT",ResourceRecordSet:{
          Name:$h,Type:$t,
          AliasTarget:{HostedZoneId:$z,DNSName:$dn,EvaluateTargetHealth:false}}}]}')" >/dev/null
  done
done

# ---- persist state for deploy.sh -------------------------------------------
cat > "${STATE}" <<ENV
# Written by bootstrap.sh — consumed by deploy.sh. Safe to commit.
BUCKET="${BUCKET}"
REGION="${REGION}"
DISTRIBUTION_ID="${DIST_ID}"
DISTRIBUTION_DOMAIN="${DIST_DOMAIN}"
ENV

say "Waiting for the distribution to finish deploying (5-15 min)..."
aws cloudfront wait distribution-deployed --id "${DIST_ID}"

say "Done. Now run:  ./deploy.sh"
echo "    Site will be live at https://${DOMAIN}/ once DNS propagates."
