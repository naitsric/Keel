//! Folders a client proposed adding to Keel. Each waits here, as a file, until the person adds or
//! discards it. None is a project until then, so nothing can start a run on one.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::workflows::safe_file;

/// Proposals that may wait at once, so a client can't bury the person in them.
pub const MAX_PROJECT_PROPOSALS: usize = 10;
const MAX_PATH_CHARS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectProposal {
    /// The file's name, which Keel picks.
    #[serde(default)]
    pub id: String,
    /// The repo's root as git reports it, which is what adding it would add.
    pub path: String,
    pub proposed_by: String,
    pub created_at: String,
}

/// Newest first. A broken file is left out, and takes no other proposal with it.
pub fn load(dir: &Path) -> io::Result<Vec<ProjectProposal>> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(mut proposal) = serde_yaml_ng::from_str::<ProjectProposal>(&text) {
            proposal.id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            found.push(proposal);
        }
    }
    found.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(found)
}

pub fn save(dir: &Path, proposal: &ProjectProposal) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_yaml_ng::to_string(proposal).map_err(io::Error::other)?;
    std::fs::write(safe_file(dir, &proposal.id)?, text)
}

/// One that's already gone is what was wanted, so that's no failure.
pub fn delete(dir: &Path, id: &str) -> io::Result<()> {
    match std::fs::remove_file(safe_file(dir, id)?) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A line break, a character that shows the text after it in another order, or one that shows as
/// nothing at all: what would keep a person from reading a value for what it is. That's every
/// control and format character Unicode says shows as nothing (Default_Ignorable_Code_Point), and
/// the line and paragraph separators. For what a client sets to run, or adds, without asking.
pub fn hides_text(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'..='\u{1160}'
                | '\u{17B4}'..='\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF0}'..='\u{FFFB}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0000}'..='\u{E0FFF}'
        )
}

/// Whether the person can read this path for what it is. The folder is what they're deciding on,
/// so it can't carry a line break, or a character that shows the text after it in another order.
pub fn readable_path(path: &str) -> bool {
    path.chars().count() <= MAX_PATH_CHARS
        && !path.chars().any(|c| {
            c.is_control() || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
}

/// `readable_path`, and none of what `hides_text` has either: for a folder a client adds without
/// the person deciding on it first.
pub fn readable_path_for_client(path: &str) -> bool {
    readable_path(path) && !path.chars().any(hides_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(id: &str, created_at: &str) -> ProjectProposal {
        ProjectProposal {
            id: id.into(),
            path: format!("/repos/{id}"),
            proposed_by: "Claude Code".into(),
            created_at: created_at.into(),
        }
    }

    #[test]
    fn they_come_back_newest_first_and_a_broken_file_takes_none_with_it() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &proposal("old", "2026-01-01T00:00:00.000Z")).unwrap();
        save(dir.path(), &proposal("new", "2026-02-01T00:00:00.000Z")).unwrap();
        std::fs::write(dir.path().join("broken.yaml"), ": not: yaml: [").unwrap();
        let ids: Vec<String> = load(dir.path())
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["new", "old"]);
    }

    #[test]
    fn no_folder_yet_means_none_are_waiting() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("nothing")).unwrap().is_empty());
    }

    #[test]
    fn discarding_cannot_reach_outside_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let waiting = dir.path().join("waiting");
        std::fs::create_dir(&waiting).unwrap();
        let outside = dir.path().join("outside.yaml");
        std::fs::write(&outside, "x").unwrap();
        assert!(delete(&waiting, "../outside").is_err());
        assert!(outside.exists());
        delete(&waiting, "already-gone").unwrap();
    }

    #[test]
    fn a_path_that_could_hide_what_it_is_is_not_readable() {
        assert!(readable_path("/Users/you/www/año nuevo"));
        assert!(!readable_path("/Users/you/a\nb"));
        assert!(!readable_path("/Users/you/\u{202E}gpj.exe"));
        assert!(!readable_path("/Users/you/\u{2066}x"));
        assert!(!readable_path(&format!("/{}", "a".repeat(MAX_PATH_CHARS))));
        // A proposal takes what it always took, as a folder in Persian or with a joined emoji.
        assert!(readable_path("/Users/you/می\u{200C}خواهم"));
        assert!(readable_path("/Users/you/👨\u{200D}💻"));
    }

    #[test]
    fn a_folder_a_client_adds_can_show_nothing_hidden() {
        assert!(readable_path_for_client("/Users/you/www/año nuevo"));
        assert!(!readable_path_for_client("/Users/you/\u{202E}gpj.exe"));
        // Ones that show as nothing, or as a plain break: the same path to the eye as another.
        for hidden in [
            '\u{00AD}',
            '\u{034F}',
            '\u{061C}',
            '\u{180E}',
            '\u{200B}',
            '\u{200D}',
            '\u{2028}',
            '\u{2029}',
            '\u{2060}',
            '\u{206A}',
            '\u{3164}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{FFA0}',
            '\u{FFF9}',
            '\u{1D173}',
            '\u{E0041}',
            '\u{E0100}',
        ] {
            assert!(
                !readable_path_for_client(&format!("/Users/you/a{hidden}b")),
                "{hidden:?}"
            );
            assert!(hides_text(hidden), "{hidden:?}");
        }
    }
}
