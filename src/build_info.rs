//! Build identity: the semver version plus a build timestamp.
//!
//! The split matters. `CARGO_PKG_VERSION` is *also* used on the wire —
//! `User-Agent`, OAuth client version, ACP `agentInfo` — and is parsed by the
//! updater as semver. Appending a timestamp to it would corrupt all of those.
//! So the timestamp lives here and is used only for **display**.

/// Plain semver from `Cargo.toml`. Never append to this.
pub const SEMVER: &str = env!("CARGO_PKG_VERSION");

/// Build timestamp baked in by `build.rs`, e.g. `2026-10-06T15:32Z`.
pub const BUILD_STAMP: &str = env!("CRABCODE_BUILD_STAMP");

/// Long form as a `&'static str`. Clap's `#[command(version = ...)]` attribute
/// requires one (it will not accept a `String`), and `concat!` keeps this a
/// compile-time constant rather than a runtime allocation.
pub const DISPLAY_VERSION: &str = if BUILD_STAMP.is_empty() {
    SEMVER
} else {
    concat!(
        env!("CARGO_PKG_VERSION"),
        " (build ",
        env!("CRABCODE_BUILD_STAMP"),
        ")"
    )
};

/// Version for human-facing output: `0.0.13 (build 2026-10-06T15:32Z)`.
///
/// Falls back to bare semver if the stamp is somehow empty, so this is always
/// safe to print.
pub fn display_version() -> String {
    DISPLAY_VERSION.to_string()
}

/// Short form for the status bar, where space is tight: `0.0.13 · 10-06 15:32`.
pub fn short_display_version() -> String {
    let stamp = BUILD_STAMP.trim();
    if stamp.is_empty() {
        return SEMVER.to_string();
    }
    // `2026-10-06T15:32Z` -> `10-06 15:32`
    let Some((date, time)) = stamp.split_once('T') else {
        return SEMVER.to_string();
    };
    let date = date
        .split_once('-')
        .and_then(|(_, rest)| rest.split_once('-'))
        .map(|(month, day)| format!("{month}-{day}"))
        .unwrap_or_else(|| date.to_string());
    let time = time.trim_end_matches('Z');
    format!("{SEMVER} · {date} {time}")
}

#[cfg(test)]
mod tests {
    use super::{display_version, short_display_version, BUILD_STAMP, SEMVER};

    #[test]
    fn semver_is_plain_and_parseable() {
        let parts: Vec<&str> = SEMVER.split('.').collect();
        assert!(
            parts.len() == 3 && parts.iter().all(|part| part.parse::<u32>().is_ok()),
            "wire-visible version must stay semver, got {SEMVER:?}"
        );
    }

    #[test]
    fn stamp_is_present_and_well_formed() {
        assert!(!BUILD_STAMP.trim().is_empty(), "build.rs must emit a stamp");
        assert!(
            BUILD_STAMP.contains('T') && BUILD_STAMP.ends_with('Z'),
            "expected ISO-8601 UTC stamp, got {BUILD_STAMP:?}"
        );
    }

    #[test]
    fn display_version_carries_both_parts() {
        let shown = display_version();
        assert!(
            shown.starts_with(SEMVER),
            "{shown} should start with {SEMVER}"
        );
        assert!(
            shown.contains(BUILD_STAMP.trim()),
            "{shown} lacks the stamp"
        );
    }

    #[test]
    fn short_form_drops_the_year_but_keeps_time() {
        let short = short_display_version();
        assert!(short.starts_with(SEMVER));
        assert!(!short.contains("2026-10-06T"), "year should be trimmed");
        assert!(
            short.len() < display_version().len(),
            "short form must be shorter than the long one"
        );
    }
}
