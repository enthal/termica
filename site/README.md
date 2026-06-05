# termica.io — homepage

A single static page for [termica.io](https://termica.io), served from a private
S3 bucket through CloudFront. No build step, no framework.

```
site/
  src/                 # exactly what gets uploaded (aws s3 sync target)
    index.html
    style.css
    icon.png           # = assets/app_icon.png (canonical source of truth)
    favicon.ico        # 48px, generated from the app icon
    favicon-16.png  favicon-32.png  apple-touch-icon.png
  infra/
    bootstrap.sh       # one-time AWS setup (run once)
    deploy.sh          # repeatable publish (sync + cache invalidation)
    config.env         # written by bootstrap.sh, read by deploy.sh
```

## Architecture

```
Route53 (termica.io, www)  --alias-->  CloudFront  --OAC-->  S3 (private)
                                            |
                              ACM cert (us-east-1, DNS-validated)
```

- The S3 bucket is **private**. CloudFront reaches it via **Origin Access
  Control (OAC)**; a bucket policy grants read access only to this one
  distribution. There is no public S3 website endpoint.
- TLS cert lives in **us-east-1** (CloudFront's only cert region).
- A **CloudFront Function** 301-redirects `www.termica.io` → `termica.io`.
- HTTP redirects to HTTPS at the edge.

## Local preview

Absolute (`/style.css`) paths need a web root, so serve it — don't open the
file directly:

```sh
cd site/src && python3 -m http.server 8787   # then open http://localhost:8787/
```

## First-time setup

Needs `aws` (v2) + `jq` and credentials with S3/ACM/CloudFront/Route53 rights.
The Route53 hosted zone for `termica.io` must already exist.

```sh
./infra/bootstrap.sh     # ~10-20 min; waits through cert + distribution deploy
./infra/deploy.sh        # upload the files
```

## Updating the site

Edit anything in `src/`, then:

```sh
./infra/deploy.sh
```

`index.html` is uploaded with `no-cache`; other assets get a 7-day cache.
Every deploy issues a `/*` CloudFront invalidation, so changes appear within a
minute.

## Regenerating the icons

All derived from `assets/app_icon.png` (256×256):

```sh
SRC=../../assets/app_icon.png
sips -z 16 16   "$SRC" --out src/favicon-16.png
sips -z 32 32   "$SRC" --out src/favicon-32.png
sips -z 180 180 "$SRC" --out src/apple-touch-icon.png
sips -z 48 48   "$SRC" --out /tmp/fav48.png
sips -s format ico /tmp/fav48.png --out src/favicon.ico
cp "$SRC" src/icon.png
```
