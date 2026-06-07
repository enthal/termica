**← Previous:** [11 — Keyboard shortcuts](11-keyboard-shortcuts.md) | **Up:** [SPEC index](../SPEC.md)

# 12 — Distribution & releases

How Termica gets from a merged PR to something a user can download and run as a
native app on macOS and Linux — without per-PR version churn, without editing the
homepage on every release, and without a heavyweight build toolchain.

This document is forward-looking: it specifies the intended pipeline. Tooling
choices marked *(provisional)* may change during implementation, but the
**principles** and the **public contracts** (stable download URLs, version
scheme) are normative once shipped.

## Principles

1. **Version is assigned at release time, never in feature PRs.** Bumping
   `Cargo.toml` in every PR causes constant merge conflicts and makes
   out-of-order merges painful. Feature PRs MUST NOT change the package version.
2. **Releases are cut on demand, not on a clock or per-PR.** A release happens
   when there is user-visible value to ship.
3. **Stable, version-free public URLs.** The homepage and README MUST link to
   "latest" through URLs that never change when the version bumps.
4. **Same artifact in CI and locally.** The packaging a maintainer can run by
   hand is the packaging CI runs. No CI-only magic.
5. **No bundler.** Termica is a Rust GUI binary, not a web app. Packaging is
   shell + a Rust packaging crate, not webpack/Vite/node.
6. **Signing is additive.** The pipeline works unsigned; code-signing /
   notarization steps activate only when their secrets are present, so they can
   be added later without restructuring.

## Versioning

- **Scheme:** Semantic Versioning. Pre-1.0 we use `0.MINOR.PATCH`: a `feat`
  bumps MINOR (`0.2.0`), a `fix` bumps PATCH (`0.1.1`). Breaking changes are
  expected pre-1.0 and ride a MINOR bump. `1.0.0` is cut when Termica is a
  daily-driver per [10 — Roadmap](10-roadmap.md) Phase 10.
- **Source of the bump:** Conventional Commit prefixes (`feat`, `fix`, `docs`,
  `refactor`, `chore`, …), which the project already uses. The commit history
  *is* the changelog input.
- **Mechanism *(provisional)*:** [`release-plz`](https://release-plz.dev) runs on
  `main` and maintains an open **Release PR** that bumps `Cargo.toml` +
  `Cargo.lock` and regenerates `CHANGELOG.md` from the conventional commits.
  - Feature PRs touch neither the version nor the changelog.
  - Out-of-order merges are a non-issue: the version is computed from the set of
    unreleased commits at release time, and `release-plz` rebuilds the Release PR
    as commits land.
  - **Cutting a release = merging the Release PR.** On merge, `release-plz` tags
    `vMAJOR.MINOR.PATCH` and creates the GitHub Release, which triggers the build
    pipeline below.

## Release pipeline

```
Conventional-commit PRs --> main
                             |
              release-plz Release PR (version + CHANGELOG)
                             | merge
                          tag v0.X.Y  +  GitHub Release (draft/published)
                             | triggers
                   .github/workflows/release.yml  (matrix build)
                             |
        per-target: build --release -> package -> (sign) -> upload to the Release
```

- **Trigger:** push of a `v*` tag (equivalently, the GitHub Release created by
  `release-plz`). The everyday `ci.yml` (fmt + clippy + test) is unchanged and
  still gates PRs.
- **Builder:** GitHub Actions, one job per target in a matrix.

### Build matrix

| OS runner | Target triple | Produces |
|---|---|---|
| `macos-14` (Apple Silicon) | `aarch64-apple-darwin` | `.app` → `.dmg` |
| `macos-13` (Intel) | `x86_64-apple-darwin` | `.app` → `.dmg` |
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | AppImage + `.deb` |

A macOS *universal* binary (`lipo` of both arches in one `.app`) is an option
instead of two `.dmg`s; the two-arch matrix is the simpler start. Linux
`aarch64` and additional package formats are deferred until requested.

## Packaging

Termica is a **GUI app**, so users expect native, double-clickable artifacts —
not a raw binary tarball. Packaging *(provisional)* via
[`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager), which
produces every format below from one config; the app icon is generated from
[`assets/app_icon.png`](../assets/app_icon.png) (→ `.icns` on macOS).

| Platform | Format | Why |
|---|---|---|
| macOS | `.app` inside a `.dmg` | Drag-to-`/Applications`; proper `Info.plist`, bundle id, icon |
| Linux | **AppImage** | Distro-agnostic single file; runs on Ubuntu and most others, no install |
| Linux | `.deb` | Native `apt` / `dpkg` install for Ubuntu/Debian users |

Flatpak, AUR, Homebrew cask, and Windows packaging are explicitly **post-MVP**
(see [10 — Roadmap](10-roadmap.md) post-MVP) and added on demand.

## Code signing & notarization

### macOS

- **Unsigned reality:** a not-notarized `.app` trips Gatekeeper —
  *"Termica can't be opened because Apple cannot check it for malware."* The user
  must right-click → Open once (or clear the quarantine attribute). Functional,
  but a friction wall for non-technical visitors.
- **Signed + notarized:** opens cleanly. Requires the **Apple Developer Program
  ($99/yr)**, a *Developer ID Application* certificate, and an App Store Connect
  API key, stored as GitHub Actions secrets; CI runs `codesign` then
  `notarytool`.
- **Decision (current):** **ship unsigned initially.** Termica's early audience
  is developers who tolerate the right-click step, and the download page MUST
  carry clear "first launch on macOS" instructions. The release workflow MUST be
  written so the signing/notarization steps run only `if` the secrets are
  present — adding the Apple account later is a drop-in, no restructuring.
  Revisit when promoting Termica beyond the developer audience.

### Linux

No signing required. AppImage runs as-is; the `.deb` may be installed with
`sudo apt install ./termica_*.deb`. GPG-signing the `.deb` / publishing an apt
repo is post-MVP.

## Distribution & download links

The public contract is a set of **version-free URLs** so the site and README
never need editing per release:

- **Releases page (canonical):** `https://github.com/enthal/termica/releases/latest`
  redirects to the newest release; the page lists every asset. Used as the README
  download link and the homepage fallback.
- **Latest-version badge:** `https://img.shields.io/github/v/release/enthal/termica`
  in the README — auto-updates.
- **Homepage smart button:** client-side JS fetches
  `https://api.github.com/repos/enthal/termica/releases/latest`, detects the
  visitor's OS/arch (`navigator.userAgentData` / `navigator.platform`), and sets
  the download button to the matching asset's URL (e.g. *"Download for macOS —
  Apple Silicon"*). Handles version-stamped asset filenames automatically and
  needs no per-release edits; falls back to the releases page if detection or the
  API call fails.
- **Asset naming:** filenames SHOULD encode platform + arch (e.g.
  `Termica_0.2.0_aarch64.dmg`, `termica_0.2.0_amd64.deb`,
  `Termica-0.2.0-x86_64.AppImage`) so a human picking from the releases page can
  tell them apart; the smart button matches on these.

The homepage MUST NOT hard-code a version string anywhere.

## Release cadence

- **On-demand stable releases** via the `release-plz` Release PR. Merge features
  continuously; cut a release by merging the Release PR when there's something
  worth shipping (pre-1.0, realistically every week or two).
- **No nightly / canary channel initially.** It is extra CI cost and another
  artifact to explain; add a `main`-tracking canary only when testers ask for
  bleeding edge.
- **Not per-PR.** Each release runs a multi-platform build and creates a Release
  entry; per-PR releases are noise.

## Merge queue

Now that the repository is public, GitHub's **merge queue** is available on the
Free plan (private repos need Team/Enterprise). It batches PRs and tests them
*together, in merge order*, blocking anything that would break `main` — the real
fix for out-of-order merges. SHOULD be enabled via branch protection on `main`
(require the queue + the `ci.yml` checks). Low urgency while there is a single
maintainer; its value scales with contributor count.

## Open decisions

| Decision | Default if unresolved |
|---|---|
| Apple Developer Program for notarization | Defer; ship unsigned, CI signing-ready |
| Two per-arch `.dmg`s vs one universal `.app` | Two-arch matrix (simpler) |
| `cargo-packager` vs hand-rolled bundling | `cargo-packager` (one config, all formats) |
| Nightly/canary channel | Skip until requested |
| Merge queue | Enable (low urgency) |

## Phased rollout

Each phase is its own PR; downloads exist after Phase 1.

1. **Release CI + versioning.** `cargo-packager` config, `release.yml` on `v*`
   tags building macOS (both arches) + Linux (AppImage + `.deb`) **unsigned**,
   and `release-plz` wiring. Cut `v0.1.0`.
2. **Download UX.** Homepage smart OS-detect button + "first launch" notes;
   README badge + `releases/latest` link + install instructions. Replaces the
   current "build from source" framing.
3. **macOS signing + notarization.** Once an Apple Developer account exists: add
   secrets + signing steps (already gated behind their presence).
4. **Merge queue** + optional canary channel.
5. **Post-MVP channels:** Homebrew cask, AUR, Flatpak, Windows — as demand appears.

---

**← Previous:** [11 — Keyboard shortcuts](11-keyboard-shortcuts.md) | **Up:** [SPEC index](../SPEC.md)
