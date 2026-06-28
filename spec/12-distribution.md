**← Previous:** [11 — Keyboard shortcuts](11-keyboard-shortcuts.md) | **Up:** [SPEC index](../SPEC.md)

# 12 — Distribution & releases

How Termica gets from a merged PR to something a user can download and run as a native app on macOS and Linux — without per-PR version churn, without editing the homepage on every release, and without a heavyweight build toolchain.

This document is forward-looking: it specifies the intended pipeline. Tooling choices marked *(provisional)* may change during implementation, but the **principles** and the **public contracts** (stable download URLs, version scheme) are normative once shipped.

## Principles

1. **Version is assigned at release time, never in feature PRs.** Bumping `Cargo.toml` in every PR causes constant merge conflicts and makes out-of-order merges painful. Feature PRs MUST NOT change the package version.
2. **Releases are cut on demand, not on a clock or per-PR.** A release happens when there is user-visible value to ship.
3. **Stable, version-free public URLs.** The homepage and README MUST link to "latest" through URLs that never change when the version bumps.
4. **Same artifact in CI and locally.** The packaging a maintainer can run by hand is the packaging CI runs. No CI-only magic.
5. **No bundler.** Termica is a Rust GUI binary, not a web app. Packaging is shell + a Rust packaging crate, not webpack/Vite/node.
6. **Signing is additive.** The pipeline works unsigned; code-signing / notarization steps activate only when their secrets are present, so they can be added later without restructuring.

## Versioning

- **Scheme:** Semantic Versioning. Pre-1.0 we use `0.MINOR.PATCH`: a `feat` bumps MINOR (`0.2.0`), a `fix` bumps PATCH (`0.1.1`). Breaking changes are expected pre-1.0 and ride a MINOR bump. `1.0.0` is cut when Termica is a daily-driver per [10 — Roadmap](10-roadmap.md) Phase 10.
- **Source of the bump:** Conventional Commit prefixes (`feat`, `fix`, `docs`, `refactor`, `chore`, …), which the project already uses. The commit history *is* the changelog input.
- **Mechanism *(provisional)*:** [`release-plz`](https://release-plz.dev) runs on `main` and maintains an open **Release PR** that bumps `Cargo.toml` + `Cargo.lock` and regenerates `CHANGELOG.md` from the conventional commits.
  - Feature PRs touch neither the version nor the changelog.
  - Out-of-order merges are a non-issue: the version is computed from the set of unreleased commits at release time, and `release-plz` rebuilds the Release PR as commits land.
  - **Cutting a release = merging the Release PR.** On merge, `release-plz` tags `vMAJOR.MINOR.PATCH` and creates the GitHub Release, which triggers the build pipeline below.

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

- **Trigger:** push of a `v*` tag (equivalently, the GitHub Release created by `release-plz`). The everyday `ci.yml` (fmt + clippy + test) is unchanged and still gates PRs.
- **Builder:** GitHub Actions, one job per target in a matrix.

### Build matrix

| OS runner | Target triple | Produces |
|---|---|---|
| `macos-14` (Apple Silicon) | `aarch64-apple-darwin` | `.app` → `.dmg` |
| `macos-13` (Intel) | `x86_64-apple-darwin` | `.app` → `.dmg` |
| `ubuntu-latest` | `x86_64-unknown-linux-gnu` | AppImage + `.deb` |

A macOS *universal* binary (`lipo` of both arches in one `.app`) is an option instead of two `.dmg`s; the two-arch matrix is the simpler start. Linux `aarch64` and additional package formats are deferred until requested.

## Packaging

Termica is a **GUI app**, so users expect native, double-clickable artifacts — not a raw binary tarball. Packaging *(provisional)* via [`cargo-packager`](https://github.com/crabnebula-dev/cargo-packager), which produces every format below from one config; the app icon is generated from [`assets/app_icon.png`](../assets/app_icon.png) (→ `.icns` on macOS).

| Platform | Format | Why |
|---|---|---|
| macOS | `.app` inside a `.dmg` | Drag-to-`/Applications`; proper `Info.plist`, bundle id, icon |
| Linux | **AppImage** | Distro-agnostic single file; runs on Ubuntu and most others, no install |
| Linux | `.deb` | Native `apt` / `dpkg` install for Ubuntu/Debian users |

Flatpak, AUR, Homebrew cask, and Windows packaging are explicitly **post-MVP** (see [10 — Roadmap](10-roadmap.md) post-MVP) and added on demand.

> **Cursor-read follow-up tied to sandboxed packaging:** when a Flatpak or Snap package ships, switch the cursor size/theme read in `src/cursor_env.rs` from the `gsettings` subprocess to the XDG `org.freedesktop.portal.Settings` portal. Inside a sandbox `gsettings` is blocked, so today's read simply no-ops there; the portal is the supported path and is a **cheap swap** — `zbus` (blocking API, pure Rust) is already in our dependency tree via `accesskit`, so no new heavyweight dependency. Non-sandboxed GNOME already works via `gsettings`, and KDE almost certainly needs nothing (Plasma exports `XCURSOR_SIZE`, which our existing no-op respects), so this is only worth doing alongside sandboxed packaging — not before.

### Desktop integration (Linux)

Linux desktops put an app's icon on its window — and merge a running window with its launcher — by matching the window's **`app_id`** to an installed `.desktop` entry of the same basename. Termica uses **one reverse-DNS identity** for all of it (`io.termica.Termica`, capitalized app component per `dev.warp.Warp` / `dev.zed.Zed`): the Wayland/X11 `app_id`, the `.desktop` and icon basenames, `Icon=`, and `StartupWMClass`. That identity **MUST equal the cargo-packager `identifier`**, or the installed launcher and the running window become two different apps (generic icon; launcher won't merge with its window). A unit test (`app_id_matches_packaged_identifier`) pins the two together.

Beyond any package-installed entry, Termica **self-installs** the `.desktop` + icon under `$XDG_DATA_HOME` on every launch — idempotent, "steal on start". This is what gives a dock icon to the **AppImage** and to dev / `cargo run` builds, where no package manager dropped an entry.

The entry's **`Exec` must be an absolute path that resolves**: GIO's `GDesktopAppInfo` loader — which gnome-shell calls to map a window's `app_id` to an app — runs `g_find_program_in_path` on `Exec` and returns NULL for the *whole entry* (icon and all) if it does not resolve. So Termica writes `std::env::current_exe()` (never a bare `termica`, which is not on `PATH` for a dev build), or, inside an AppImage, the stable `$APPIMAGE` file path rather than the per-launch `/tmp/.mount_*` (guarded by `$APPDIR` membership so an inherited `$APPIMAGE` from a parent process is not trusted). The storage namespace stays the short `termica` (history DB, eframe state), distinct from the desktop identity, so user data does not move when the identity changes.

Listing in software centers (an AppStream `io.termica.Termica.metainfo.xml`) is a post-MVP follow-up.

## Code signing & notarization

### macOS

- **Unsigned reality:** a not-notarized `.app` trips Gatekeeper — *"Apple could not verify 'Termica' is free of malware."* The user must go to System Settings → Privacy & Security → *Open Anyway* (or clear the quarantine attribute). Functional, but a friction wall for non-technical visitors.
- **Signed + notarized (current):** opens cleanly, even offline. Requires the **Apple Developer Program ($99/yr)**, a *Developer ID Application* certificate, and an App Store Connect API key, stored as GitHub Actions secrets.
- **Decision (current): sign + notarize.** Termica has an enrolled Apple Developer account (Team `V64896A4F2`). The release workflow signs and notarizes whenever the Apple secrets are present (`HAS_APPLE_SIGNING`), and still produces working **unsigned** artifacts when they are not (e.g. forks) — so the signing path is additive, never load-bearing for a successful build.

#### Mechanism (normative)

The `.p12` stored in `APPLE_CERTIFICATE` MUST be a **full chain** — the *Developer ID Application* leaf, the **Developer ID Certification Authority (G2)** intermediate, and the private key — base64-encoded. GitHub's fresh keychains do not carry the G2 intermediate, so a leaf-only `.p12` yields "0 valid identities" and signing fails; bundling the intermediate makes the chain self-validate against the system roots.

Two stages, both on the macOS runners only:

1. **Sign (cargo-packager).** cargo-packager only runs its signing path when `macos.signing-identity` is set in config — `APPLE_CERTIFICATE` alone is ignored. The workflow injects that field into `Cargo.toml` in a step gated on the secrets (the identity string is the cert's CommonName, not a secret), so forks without the cert still build unsigned. Then, with `APPLE_CERTIFICATE` + `APPLE_CERTIFICATE_PASSWORD` in the environment, `cargo packager` imports the `.p12` into a temporary keychain and runs `codesign --options runtime --timestamp` (hardened runtime + secure timestamp — notarization prerequisites), building the `.dmg` around the signed `.app`.
2. **Notarize + staple (`notarytool` / `stapler`).** A dedicated step submits each `.dmg` to Apple's notary service using the App Store Connect API key (`APPLE_API_KEY_P8` / `APPLE_API_KEY_ID` / `APPLE_API_ISSUER`) with `--wait`, then `xcrun stapler staple`s the ticket. `stapler` is the safety gate: it can only succeed on an *Accepted* notarization, so a rejected build fails the job rather than shipping.

| GitHub Actions secret | Purpose |
| --- | --- |
| `APPLE_CERTIFICATE` | base64 of the full-chain Developer ID `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the `.p12` export password |
| `APPLE_API_KEY_P8` | contents of the App Store Connect `AuthKey_*.p8` |
| `APPLE_API_KEY_ID` | the API key's Key ID |
| `APPLE_API_ISSUER` | the App Store Connect Issuer ID |

The signed `.app` inside the `.dmg` carries Apple's notarization (recorded by hash); stapling the enclosing `.dmg` is what makes a freshly-downloaded image open without a prompt offline. Stapling the `.app` itself (for the copy-out-then-launch-offline edge case) is a possible future refinement.

### Linux

No signing required. AppImage runs as-is; the `.deb` may be installed with `sudo apt install ./termica_*.deb`. GPG-signing the `.deb` / publishing an apt repo is post-MVP.

#### Cursor size & theme

winit decides the mouse pointer's size and theme **solely** from the `XCURSOR_SIZE` / `XCURSOR_THEME` environment variables — on Wayland it loads the X cursor theme itself (there are no server-side cursors), and it does not read GSettings or the XDG settings portal the way GTK and Electron do. A user who enlarged their pointer (GNOME "Cursor Size" / accessibility magnification) would therefore see a default-size cursor in Termica while other apps honor the preference.

At startup, before the event loop, Termica reads the desktop's configured cursor size and theme (today via GNOME `gsettings`; the cross-desktop `org.freedesktop.portal.Settings` portal is the follow-up) and, when the env vars are not already set, **re-exec**s itself with them populated so winit picks them up. Re-exec is used instead of `std::env::set_var` because the latter is `unsafe` under edition 2024 and the crate is `#![forbid(unsafe_code)]`; it runs at most once per launch and is a no-op when the vars are already set or the desktop reports nothing. An explicit `XCURSOR_SIZE` / `XCURSOR_THEME` is always honored and never overwritten.

The GSettings probe is gated on a GNOME-family desktop (`XDG_CURRENT_DESKTOP` contains `GNOME`/`Unity`) so we never inject a GNOME-schema value on a KDE/XFCE/etc. desktop that merely has the schemas installed; those desktops fall through untouched. The bridge fails closed everywhere else too — no `gsettings` binary, a missing schema, or a sandbox that blocks it all resolve to "no change". See `src/cursor_env.rs`. A future winit that drives `wp_cursor_shape_v1` would let the compositor size the pointer and retire this bridge.

## Distribution & download links

The public contract is a set of **version-free URLs** so the site and README never need editing per release:

- **Releases page (canonical):** `https://github.com/enthal/termica/releases/latest` redirects to the newest release; the page lists every asset. Used as the README download link and the homepage fallback.
- **Latest-version badge:** `https://img.shields.io/github/v/release/enthal/termica` in the README — auto-updates.
- **Homepage smart button:** client-side JS fetches `https://api.github.com/repos/enthal/termica/releases/latest`, detects the visitor's OS/arch (`navigator.userAgentData` / `navigator.platform`), and sets the download button to the matching asset's URL (e.g. *"Download for macOS — Apple Silicon"*). Handles version-stamped asset filenames automatically and needs no per-release edits; falls back to the releases page if detection or the API call fails.
- **Asset naming:** filenames SHOULD encode platform + arch (e.g. `Termica_0.2.0_aarch64.dmg`, `termica_0.2.0_amd64.deb`, `Termica-0.2.0-x86_64.AppImage`) so a human picking from the releases page can tell them apart; the smart button matches on these.

The homepage MUST NOT hard-code a version string anywhere.

## Release cadence

- **On-demand stable releases** via the `release-plz` Release PR. Merge features continuously; cut a release by merging the Release PR when there's something worth shipping (pre-1.0, realistically every week or two).
- **No nightly / canary channel initially.** It is extra CI cost and another artifact to explain; add a `main`-tracking canary only when testers ask for bleeding edge.
- **Not per-PR.** Each release runs a multi-platform build and creates a Release entry; per-PR releases are noise.

## Merge queue

Now that the repository is public, GitHub's **merge queue** is available on the Free plan (private repos need Team/Enterprise). It batches PRs and tests them *together, in merge order*, blocking anything that would break `main` — the real fix for out-of-order merges. SHOULD be enabled via branch protection on `main` (require the queue + the `ci.yml` checks). Low urgency while there is a single maintainer; its value scales with contributor count.

## Open decisions

| Decision | Default if unresolved |
|---|---|
| Apple Developer Program for notarization | Resolved: enrolled; CI signs + notarizes (Team `V64896A4F2`) |
| Two per-arch `.dmg`s vs one universal `.app` | Two-arch matrix (simpler) |
| `cargo-packager` vs hand-rolled bundling | `cargo-packager` (one config, all formats) |
| Nightly/canary channel | Skip until requested |
| Merge queue | Enable (low urgency) |

## Phased rollout

Each phase is its own PR; downloads exist after Phase 1.

1. **Release CI + versioning.** ✅ `cargo-packager` config, `release.yml` on `v*` tags building macOS (both arches) + Linux (AppImage + `.deb`), and `release-plz` wiring.
2. **macOS signing + notarization.** ✅ Apple Developer account enrolled; CI signs (full-chain `.p12`) and notarizes + staples the `.dmg`, gated behind `HAS_APPLE_SIGNING`. Cut a signed `v0.1.0`.
3. **Download UX.** Homepage smart OS-detect button; README `releases/latest` links + install instructions. Replaces the current "build from source" framing.
4. **Merge queue** + optional canary channel.
5. **Post-MVP channels:** Homebrew cask, AUR, Flatpak, Windows — as demand appears. When Flatpak/Snap lands, also do the cursor-read portal swap noted under [Packaging](#packaging).

---

**← Previous:** [11 — Keyboard shortcuts](11-keyboard-shortcuts.md) | **Up:** [SPEC index](../SPEC.md)
