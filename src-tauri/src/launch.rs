//! What's settled before the app has its data: what a build from a branch keeps apart from the
//! installed Keel, and telling the person why when the data can't be opened.

use std::path::Path;

use keel_store::Error as StoreError;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

/// A debug build is what `pnpm app` gives: a Keel built from whatever branch is checked out. Its
/// database can be ahead of the installed Keel's, or simply different, so the two share nothing
/// that either writes: see `identifier`, `home_folder`, `mcp_server_name` and
/// `keel_mcp::DEFAULT_PORT`.
pub const BRANCH_BUILD: bool = cfg!(debug_assertions);

/// The identifier names the data folder, where the database is, the lock that keeps a second copy
/// from opening, and where the window's place is kept. A branch build gets all three of its own,
/// so it can run next to the installed Keel and never opens its database.
pub fn identifier(installed: &str, branch_build: bool) -> String {
    if branch_build {
        format!("{installed}.dev")
    } else {
        installed.to_owned()
    }
}

/// The folder in the person's home where runs work and the hook lives. A branch build's runs
/// would be nothing the installed Keel knows of, so they don't go in its folder.
pub const fn home_folder(branch_build: bool) -> &'static str {
    if branch_build { ".keel-dev" } else { ".keel" }
}

/// What Claude Code lists the MCP server as. Connecting replaces the entry with this name in
/// `~/.claude.json`, so under one name a branch build would take the installed Keel's token away.
pub const fn mcp_server_name(branch_build: bool) -> &'static str {
    if branch_build { "keel-dev" } else { "keel" }
}

const EN: &str = include_str!("../../src/i18n/en.json");
const ES: &str = include_str!("../../src/i18n/es.json");
const LANGUAGE_KEY: &str = "language";

/// Why the data couldn't be opened, as far as the person can do something about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// A newer Keel used it.
    Newer,
    /// A build that disagrees with this one about a migration used it.
    Different,
    Other,
}

impl From<&StoreError> for Why {
    fn from(error: &StoreError) -> Self {
        match error {
            StoreError::FromNewerKeel { .. } => Self::Newer,
            StoreError::FromAnotherKeel { .. } => Self::Different,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// A sentence of `cantOpen` from the UI's own translations, so these are written and checked
/// where every other text is.
fn sentence(language: &str, key: &str) -> String {
    let translations = if language == "es" { ES } else { EN };
    serde_json::from_str::<serde_json::Value>(translations)
        .ok()
        .and_then(|all| all["cantOpen"][key].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// What the person reads when Keel can't open its data in `folder`. `detail` is what the system
/// said, shown only when Keel has nothing better to say.
pub fn cant_open(language: &str, why: Why, detail: &str, folder: &Path) -> Notice {
    let reason = match why {
        Why::Newer => sentence(language, "newer"),
        Why::Different => sentence(language, "different"),
        Why::Other => sentence(language, "other").replace("{{detail}}", detail),
    };
    let place = sentence(language, "where").replace("{{folder}}", &folder.to_string_lossy());
    Notice {
        title: sentence(language, "title"),
        body: format!("{reason}\n\n{place}"),
    }
}

/// The language the person chose, read from the database without opening it for use. Failing
/// that, the Mac's, as the UI does before anyone chooses.
pub fn language(database: &Path) -> &'static str {
    choose_language(
        keel_store::saved_setting(database, LANGUAGE_KEY).as_deref(),
        system_languages().as_deref(),
    )
}

fn choose_language(saved: Option<&str>, system: Option<&str>) -> &'static str {
    let spanish = match saved {
        Some("es") => true,
        Some("en") => false,
        _ => system
            .and_then(first_language)
            .is_some_and(|first| first.to_lowercase().starts_with("es")),
    };
    if spanish { "es" } else { "en" }
}

/// What macOS has under Language & Region, as `defaults` prints it.
fn system_languages() -> Option<String> {
    let output = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The first of a list like `(\n    "es-CO",\n    "en-US"\n)`.
fn first_language(listed: &str) -> Option<&str> {
    listed
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .find(|word| !word.is_empty())
}

/// Says why Keel can't open, over its window, and closes Keel when the person has read it. The
/// app goes on without its state until then, so nothing it's asked meanwhile can do anything.
pub fn tell_and_quit(app: &tauri::App, notice: Notice) {
    let handle = app.handle().clone();
    let mut dialog = app
        .dialog()
        .message(notice.body)
        .title(notice.title)
        .kind(MessageDialogKind::Error);
    if let Some(window) = app.get_webview_window("main") {
        dialog = dialog.parent(&window);
    }
    dialog.show(move |_| handle.exit(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALLED: &str = "io.github.naitsric.keel";
    const FOLDER: &str = "/Users/me/Library/Application Support/io.github.naitsric.keel";

    #[test]
    fn a_branch_build_shares_no_folder_lock_or_name_with_the_installed_keel() {
        assert_eq!(identifier(INSTALLED, false), INSTALLED);
        assert_eq!(identifier(INSTALLED, true), "io.github.naitsric.keel.dev");
        assert_eq!(home_folder(false), ".keel");
        assert_ne!(home_folder(true), home_folder(false));
        assert_eq!(mcp_server_name(false), "keel");
        assert_ne!(mcp_server_name(true), mcp_server_name(false));
        // A space would break the hook's path, and a run's scripts.
        assert!(!home_folder(true).contains(char::is_whitespace));
    }

    #[test]
    fn data_a_newer_keel_used_is_explained_in_both_languages() {
        let error = StoreError::FromNewerKeel {
            found: 24,
            known: 23,
        };
        let en = cant_open(
            "en",
            Why::from(&error),
            &error.to_string(),
            Path::new(FOLDER),
        );
        assert_eq!(en.title, "Keel can't open its data");
        assert!(en.body.contains("A newer Keel"), "{}", en.body);
        assert!(en.body.contains("Nothing was changed"), "{}", en.body);
        assert!(en.body.contains(FOLDER), "{}", en.body);

        let es = cant_open(
            "es",
            Why::from(&error),
            &error.to_string(),
            Path::new(FOLDER),
        );
        assert_eq!(es.title, "Keel no puede abrir sus datos");
        assert!(es.body.contains("más nuevo"), "{}", es.body);
        assert!(es.body.contains(FOLDER), "{}", es.body);
        assert_ne!(en.body, es.body);
    }

    #[test]
    fn data_another_build_changed_is_explained_in_both_languages() {
        let error = StoreError::FromAnotherKeel { number: 24 };
        let en = cant_open(
            "en",
            Why::from(&error),
            &error.to_string(),
            Path::new(FOLDER),
        );
        assert!(
            en.body.contains("A different version of Keel"),
            "{}",
            en.body
        );
        let es = cant_open(
            "es",
            Why::from(&error),
            &error.to_string(),
            Path::new(FOLDER),
        );
        assert!(es.body.contains("Otra versión de Keel"), "{}", es.body);
    }

    #[test]
    fn anything_else_says_what_the_system_said() {
        let error = StoreError::NotFound("made-up reason".into());
        assert_eq!(Why::from(&error), Why::Other);
        for language in ["en", "es"] {
            let notice = cant_open(language, Why::Other, &error.to_string(), Path::new(FOLDER));
            assert!(notice.body.contains("made-up reason"), "{}", notice.body);
            assert!(notice.body.contains(FOLDER), "{}", notice.body);
        }
    }

    #[test]
    fn no_notice_is_left_with_a_blank_to_fill_or_without_words() {
        for language in ["en", "es", "fr"] {
            for why in [Why::Newer, Why::Different, Why::Other] {
                let notice = cant_open(language, why, "detail", Path::new(FOLDER));
                assert!(!notice.title.is_empty());
                assert!(!notice.body.contains("{{"), "{}", notice.body);
                assert!(notice.body.lines().all(|line| line.trim() == line));
                assert!(notice.body.len() > FOLDER.len() + 40, "{}", notice.body);
            }
        }
    }

    #[test]
    fn the_notice_is_in_the_language_the_person_chose_and_failing_that_the_macs() {
        let mac_in_spanish = "(\n    \"es-CO\",\n    \"en-US\"\n)\n";
        let mac_in_english = "(\n    \"en-US\",\n    \"es-CO\"\n)\n";
        assert_eq!(choose_language(Some("es"), Some(mac_in_english)), "es");
        assert_eq!(choose_language(Some("en"), Some(mac_in_spanish)), "en");
        assert_eq!(choose_language(None, Some(mac_in_spanish)), "es");
        assert_eq!(choose_language(None, Some(mac_in_english)), "en");
        assert_eq!(choose_language(Some("fr"), Some(mac_in_spanish)), "es");
        assert_eq!(choose_language(None, None), "en");
        assert_eq!(choose_language(None, Some("")), "en");
    }

    #[test]
    fn a_database_a_newer_keel_used_gets_a_notice_in_the_persons_language_and_stays_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        keel_store::Store::open(&path)
            .unwrap()
            .set_setting(LANGUAGE_KEY, "es")
            .unwrap();
        // As a Keel with more migrations than this one leaves it.
        let newer = rusqlite::Connection::open(&path).unwrap();
        newer.pragma_update(None, "user_version", 999).unwrap();
        drop(newer);

        let Err(error) = keel_store::Store::open(&path) else {
            panic!("it opened data from a newer Keel")
        };
        let notice = cant_open(
            language(&path),
            Why::from(&error),
            &error.to_string(),
            dir.path(),
        );

        assert_eq!(notice.title, "Keel no puede abrir sus datos");
        assert!(notice.body.contains("Un Keel más nuevo"), "{}", notice.body);
        assert!(notice.body.contains(&*dir.path().to_string_lossy()));
        let after = rusqlite::Connection::open(&path).unwrap();
        let version: i64 = after
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 999);
    }
}
