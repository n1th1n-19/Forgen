//! GSettings binding.
//!
//! The schema is the source of truth for defaults and ranges — duplicating them
//! in Rust would mean two places to change and one of them silently winning.
//! Everything here reads from `gio::Settings`.

use gtk::gio;
use gtk::prelude::*;

use crate::APP_ID;

/// Open the application's settings.
///
/// Returns `None` when the schema is not installed, which is the normal state
/// for `cargo run` before `meson install`. Falling back to defaults beats
/// `gio::Settings::new` aborting the process, which is what it does on a
/// missing schema — an abort with no message that reads like a GTK bug.
pub fn open() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(APP_ID, true)?;
    Some(gio::Settings::new(APP_ID))
}

/// Rows the history model keeps materialized. Falls back to the same default
/// the schema declares.
pub fn commit_row_budget(settings: Option<&gio::Settings>) -> usize {
    settings
        .map(|s| s.int("commit-row-budget") as usize)
        .filter(|n| *n > 0)
        .unwrap_or(512)
}

pub fn cache_max_age(settings: Option<&gio::Settings>) -> std::time::Duration {
    let days = settings.map(|s| s.int("cache-max-age-days")).unwrap_or(14);
    std::time::Duration::from_secs(days.max(0) as u64 * 24 * 3600)
}

/// Restore geometry, and persist it as the user changes it.
///
/// Bound rather than saved on close: a window closed by a compositor crash or a
/// SIGKILL never runs a close handler, and losing geometry every time is the
/// kind of small wrongness that makes an app feel unfinished.
pub fn bind_window(settings: Option<&gio::Settings>, window: &adw::ApplicationWindow) {
    let Some(settings) = settings else { return };

    window.set_default_size(settings.int("window-width"), settings.int("window-height"));
    if settings.boolean("window-maximized") {
        window.maximize();
    }

    settings
        .bind("window-width", window, "default-width")
        .build();
    settings
        .bind("window-height", window, "default-height")
        .build();
    settings
        .bind("window-maximized", window, "maximized")
        .build();
}

/// Push a path onto the recent list, most recent first, deduplicated.
pub fn push_recent(settings: Option<&gio::Settings>, path: &std::path::Path) {
    let Some(settings) = settings else { return };
    let path = path.to_string_lossy().into_owned();

    let mut recent: Vec<String> = settings
        .strv("recent-repositories")
        .into_iter()
        .map(|s| s.to_string())
        .filter(|p| *p != path)
        .collect();
    recent.insert(0, path);
    recent.truncate(10);

    let refs: Vec<&str> = recent.iter().map(String::as_str).collect();
    settings.set_strv("recent-repositories", refs).ok();
}

pub fn recent(settings: Option<&gio::Settings>) -> Vec<std::path::PathBuf> {
    settings
        .map(|s| {
            s.strv("recent-repositories")
                .into_iter()
                .map(|p| std::path::PathBuf::from(p.as_str()))
                // A path in the list may have been deleted or unmounted since.
                .filter(|p| p.exists())
                .collect()
        })
        .unwrap_or_default()
}
