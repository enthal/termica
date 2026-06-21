//! Bridge the desktop's cursor size / theme preference into the
//! `XCURSOR_SIZE` / `XCURSOR_THEME` environment variables at startup.
//!
//! ## Why this exists
//!
//! On Linux, winit loads the X cursor theme itself — Wayland has no
//! server-side cursors, and X11 goes through libXcursor — and decides
//! the pointer's size and theme **solely** from the `XCURSOR_SIZE`
//! and `XCURSOR_THEME` environment variables. It does not consult
//! GSettings or the XDG settings portal the way GTK and Electron do.
//! So a user who enlarged their pointer (GNOME "Cursor Size" /
//! accessibility magnification, stored in
//! `org.gnome.desktop.interface cursor-size`) gets a correctly
//! magnified, correctly themed cursor in Chrome and VS Code but a
//! default-size, default-theme one in Termica.
//!
//! ## What we do
//!
//! At the very start of [`crate::run`] we read the desktop's
//! configured cursor size and theme and, when the corresponding env
//! var is not already set, **re-exec** ourselves with the vars
//! populated so winit picks them up when it initializes the pointer.
//!
//! We re-exec rather than calling `std::env::set_var` because the
//! latter is `unsafe` under edition 2024 and every Termica crate is
//! `#![forbid(unsafe_code)]`. Re-exec keeps the same PID and argv and
//! costs one extra `execve`; it happens at most once per launch:
//! after the re-exec the vars are set, so [`resolve_cursor_overrides`]
//! resolves them to `None` and we do not loop.
//!
//! The source of truth today is GNOME GSettings (the `gsettings`
//! binary). The cross-desktop XDG settings portal
//! (`org.freedesktop.portal.Settings`) is the documented follow-up;
//! see [spec/12 §Linux](../spec/12-distribution.md). A future winit
//! that drives `wp_cursor_shape_v1` would let the compositor size the
//! pointer and make this bridge unnecessary.

const SIZE_VAR: &str = "XCURSOR_SIZE";
const THEME_VAR: &str = "XCURSOR_THEME";

/// What to inject into the environment, computed purely from the
/// current env and the desktop-reported values. `None` means "leave
/// the existing environment untouched" — either because the var is
/// already set (an explicit user/session choice we must not clobber)
/// or because the desktop reported nothing usable.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CursorEnvOverrides {
    pub size: Option<String>,
    pub theme: Option<String>,
}

impl CursorEnvOverrides {
    pub fn is_empty(&self) -> bool {
        self.size.is_none() && self.theme.is_none()
    }
}

/// Decide what to export. An env var already set to a non-empty value
/// always wins. Otherwise we adopt the desktop value when it is
/// usable (`size > 0`, non-empty theme).
///
/// Because every var we ultimately export is one that was *not*
/// already set, a second pass over the post-re-exec environment
/// resolves all of them to `None` — this is what guarantees the
/// re-exec runs at most once.
pub fn resolve_cursor_overrides(
    env_size: Option<&str>,
    env_theme: Option<&str>,
    os_size: Option<u32>,
    os_theme: Option<&str>,
) -> CursorEnvOverrides {
    let size = if env_size.is_some_and(|s| !s.trim().is_empty()) {
        None
    } else {
        os_size.filter(|n| *n > 0).map(|n| n.to_string())
    };
    let theme = if env_theme.is_some_and(|t| !t.trim().is_empty()) {
        None
    } else {
        os_theme.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
    };
    CursorEnvOverrides { size, theme }
}

/// True when the running desktop is GNOME-family and therefore stores
/// its cursor preference in the `org.gnome.desktop.interface` schema
/// we read (GNOME, Unity, and the `ubuntu:GNOME`-style composites).
///
/// We gate the GSettings probe on this so we never inject a
/// GNOME-schema value on a KDE/XFCE/etc. desktop that merely has the
/// GNOME schemas installed but keeps its real cursor preference
/// elsewhere — there, reading the schema could hand back a stale
/// default and override the user's actual theme. Such desktops fall
/// through as a no-op, exactly as before this bridge existed. The
/// cross-desktop XDG settings portal is the eventual way to read the
/// active desktop's value regardless of family.
pub fn is_gnome_family_desktop(xdg_current_desktop: Option<&str>) -> bool {
    xdg_current_desktop.is_some_and(|value| {
        value
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("GNOME") || name.eq_ignore_ascii_case("Unity"))
    })
}

/// Parse what `gsettings get org.gnome.desktop.interface cursor-size`
/// prints — a bare integer like `96\n`.
pub fn parse_gsettings_size(raw: &str) -> Option<u32> {
    raw.trim().parse().ok()
}

/// Parse what `gsettings get … cursor-theme` prints — a
/// single-quoted string like `'Yaru'\n`.
pub fn parse_gsettings_theme(raw: &str) -> Option<String> {
    let theme = raw.trim().trim_matches('\'').trim();
    if theme.is_empty() { None } else { Some(theme.to_string()) }
}

/// Read the desktop's cursor size/theme and, when they are not
/// already exported, re-exec this process with them set so winit
/// honors the user's pointer preference. No-op when nothing needs to
/// change or when the desktop reports nothing usable.
#[cfg(target_os = "linux")]
pub fn seed_cursor_env_via_reexec() {
    use std::os::unix::process::CommandExt;

    let env_size = std::env::var(SIZE_VAR).ok();
    let env_theme = std::env::var(THEME_VAR).ok();

    // Fast path: both already set. This is also the state we land in
    // after a re-exec, so it short-circuits the (at most one) re-exec
    // before we pay for any `gsettings` subprocess.
    let both_set = env_size.as_deref().is_some_and(|s| !s.trim().is_empty())
        && env_theme.as_deref().is_some_and(|s| !s.trim().is_empty());
    if both_set {
        return;
    }

    // Only trust the GNOME schema on a GNOME-family desktop; elsewhere
    // we leave the environment untouched rather than risk injecting a
    // value that contradicts the desktop's real preference.
    if !is_gnome_family_desktop(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref()) {
        return;
    }

    let os_size = gsettings("org.gnome.desktop.interface", "cursor-size")
        .as_deref()
        .and_then(parse_gsettings_size);
    let os_theme = gsettings("org.gnome.desktop.interface", "cursor-theme")
        .as_deref()
        .and_then(parse_gsettings_theme);

    let overrides = resolve_cursor_overrides(
        env_size.as_deref(),
        env_theme.as_deref(),
        os_size,
        os_theme.as_deref(),
    );
    if overrides.is_empty() {
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1));
    if let Some(size) = overrides.size {
        cmd.env(SIZE_VAR, size);
    }
    if let Some(theme) = overrides.theme {
        cmd.env(THEME_VAR, theme);
    }
    // `exec` replaces this image and only returns on failure. On
    // failure we continue with the default cursor rather than abort
    // startup — a wrong-size pointer is a cosmetic regression, not a
    // reason to refuse to launch.
    let err = cmd.exec();
    eprintln!("termica: cursor-env re-exec failed, continuing with default cursor: {err}");
}

/// Read one GSettings key via the `gsettings` CLI. Returns `None` if
/// the binary is missing (non-GNOME desktop) or the key is unset.
#[cfg(target_os = "linux")]
fn gsettings(schema: &str, key: &str) -> Option<String> {
    let output =
        std::process::Command::new("gsettings").args(["get", schema, key]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_env_adopts_os_values() {
        let got = resolve_cursor_overrides(None, None, Some(96), Some("Yaru"));
        assert_eq!(got.size.as_deref(), Some("96"));
        assert_eq!(got.theme.as_deref(), Some("Yaru"));
    }

    #[test]
    fn empty_env_string_counts_as_unset() {
        // winit treats an empty XCURSOR_SIZE the same as unset; so do we.
        let got = resolve_cursor_overrides(Some("  "), Some(""), Some(48), Some("Adwaita"));
        assert_eq!(got.size.as_deref(), Some("48"));
        assert_eq!(got.theme.as_deref(), Some("Adwaita"));
    }

    #[test]
    fn existing_env_is_never_clobbered() {
        // An explicit session/user value must win over the desktop's.
        let got = resolve_cursor_overrides(Some("24"), Some("Bibata"), Some(96), Some("Yaru"));
        assert!(got.is_empty(), "explicit env must suppress all overrides, got {got:?}");
    }

    #[test]
    fn post_reexec_environment_is_a_fixed_point() {
        // After we export the resolved values they read back as set,
        // so a second resolve yields nothing — this is the property
        // that bounds the re-exec to a single hop.
        let first = resolve_cursor_overrides(None, None, Some(96), Some("Yaru"));
        let second = resolve_cursor_overrides(
            first.size.as_deref(),
            first.theme.as_deref(),
            Some(96),
            Some("Yaru"),
        );
        assert!(second.is_empty(), "second pass must be a no-op, got {second:?}");
    }

    #[test]
    fn missing_or_invalid_os_values_change_nothing() {
        assert!(resolve_cursor_overrides(None, None, None, None).is_empty());
        // A zero size is not usable.
        assert_eq!(resolve_cursor_overrides(None, None, Some(0), None).size, None);
        // A blank theme is not usable.
        assert_eq!(resolve_cursor_overrides(None, None, None, Some("   ")).theme, None);
    }

    #[test]
    fn gnome_family_detection() {
        assert!(is_gnome_family_desktop(Some("GNOME")));
        assert!(is_gnome_family_desktop(Some("ubuntu:GNOME"))); // Tim's session
        assert!(is_gnome_family_desktop(Some("pop:GNOME")));
        assert!(is_gnome_family_desktop(Some("gnome"))); // case-insensitive
        assert!(is_gnome_family_desktop(Some("Unity")));
        // Non-GNOME desktops must fall through to a no-op.
        assert!(!is_gnome_family_desktop(Some("KDE")));
        assert!(!is_gnome_family_desktop(Some("XFCE")));
        assert!(!is_gnome_family_desktop(Some("sway:wlroots")));
        assert!(!is_gnome_family_desktop(None));
        assert!(!is_gnome_family_desktop(Some("")));
    }

    #[test]
    fn gsettings_size_parsing() {
        assert_eq!(parse_gsettings_size("96\n"), Some(96));
        assert_eq!(parse_gsettings_size("  32 "), Some(32));
        assert_eq!(parse_gsettings_size("uint32 24"), None); // we read the plain-int schema
        assert_eq!(parse_gsettings_size("nonsense"), None);
    }

    #[test]
    fn gsettings_theme_parsing() {
        assert_eq!(parse_gsettings_theme("'Yaru'\n").as_deref(), Some("Yaru"));
        assert_eq!(parse_gsettings_theme("'Adwaita'").as_deref(), Some("Adwaita"));
        assert_eq!(parse_gsettings_theme("''"), None);
        assert_eq!(parse_gsettings_theme("   "), None);
    }
}
