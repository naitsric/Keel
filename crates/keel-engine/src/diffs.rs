//! How a change reaches an agent's prompt. One that fits goes in whole, as it always has. A bigger
//! one is also saved one file per changed file, outside the workspace, where the agent can read it
//! and change nothing. The prompt then lists every changed file and says where to read each one it
//! leaves out, so nothing is ever cut without saying what and where the rest is.

use std::io;
use std::path::{Path, PathBuf};

use keel_harness::process;

/// How much of a change goes into a prompt.
const PROMPT_LIMIT: usize = 200 * 1024;
/// Past this, the list of changed files goes in a file of its own too.
const LIST_LIMIT: usize = 40 * 1024;
/// The list of changed files, saved with them, for when it doesn't fit in the prompt either.
const LIST_FILE: &str = "files.txt";

/// One changed file's part of a diff.
#[derive(Debug, Clone, PartialEq)]
struct FileDiff {
    path: String,
    added: usize,
    removed: usize,
    text: String,
}

/// A diff split at each file. Each part starts with its own `diff --git` line: a line inside a hunk
/// starts with a space, `+`, `-` or `\`, so it can't look like one.
fn split(diff: &str) -> Vec<FileDiff> {
    let mut parts: Vec<String> = Vec::new();
    for line in diff.split_inclusive('\n') {
        match parts.last_mut() {
            Some(part) if !line.starts_with("diff --git ") => part.push_str(line),
            _ => parts.push(line.to_owned()),
        }
    }
    parts
        .into_iter()
        .map(|text| {
            let (added, removed) = counts(&text);
            FileDiff {
                path: path_of(&text),
                added,
                removed,
                text,
            }
        })
        .collect()
}

fn counts(text: &str) -> (usize, usize) {
    let (mut added, mut removed, mut in_hunk) = (0, 0, false);
    for line in text.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') {
            added += 1;
        } else if in_hunk && line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// The file a part is about, as it is after the change, or as it was if it was removed.
fn path_of(text: &str) -> String {
    let header: Vec<&str> = text.lines().take_while(|l| !l.starts_with("@@")).collect();
    let named = |prefix: &str, side: &str| {
        header.iter().find_map(|line| {
            let name = line.strip_prefix(prefix)?;
            // Git ends the name with a tab when it has a space in it.
            let name = name.strip_suffix('\t').unwrap_or(name);
            (name != "/dev/null").then(|| {
                let name = unquote(name);
                name.strip_prefix(side).map_or(name.clone(), str::to_owned)
            })
        })
    };
    if let Some(path) = named("+++ ", "b/")
        .or_else(|| named("--- ", "a/"))
        .or_else(|| named("rename to ", ""))
        .or_else(|| named("copy to ", ""))
    {
        return path;
    }
    // No content: only its mode changed, or it's binary. `a/X b/X` when it didn't move.
    let first = header
        .first()
        .and_then(|line| line.strip_prefix("diff --git "))
        .unwrap_or_default();
    let half = first.len().saturating_sub(5) / 2;
    if !first.starts_with('"')
        && first.len() == half * 2 + 5
        && first.starts_with("a/")
        && first.get(2 + half..5 + half) == Some(" b/")
        && first.get(2..2 + half) == first.get(5 + half..)
    {
        return first[2..2 + half].to_owned();
    }
    first.to_owned()
}

/// A name as git writes it when it has unusual characters in it: in quotes, with escapes.
fn unquote(name: &str) -> String {
    let Some(inner) = name
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return name.to_owned();
    };
    let mut bytes = Vec::new();
    let mut rest = inner.bytes().peekable();
    while let Some(byte) = rest.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        match rest.next() {
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match rest.peek() {
                        Some(&next @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(next - b'0');
                            rest.next();
                        }
                        _ => break,
                    }
                }
                bytes.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some(other) => bytes.push(other),
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where one file's diff is saved: numbered, so two files with the same name never meet, and with
/// only plain characters, so nothing in a name can reach outside the folder.
fn file_name(index: usize, path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let plain: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    format!("{:03}-{plain}.diff", index + 1)
}

struct Text {
    too_big: &'static str,
    others_saved: &'static str,
    others_not_saved: &'static str,
    read_all: &'static str,
    read_some: &'static str,
    list_elsewhere: &'static str,
    heading: &'static str,
    left_out: &'static str,
}

fn text(language: &str) -> Text {
    if language.starts_with("es") {
        Text {
            too_big: "Este cambio es demasiado grande para caber aquí entero: {files} archivos, +{added} -{removed} líneas. Abajo van los diffs que caben.",
            others_saved: "Los demás están marcados «no está abajo», cada uno con el archivo que lo tiene, que puedes leer pero no cambiar.",
            others_not_saved: "Keel no pudo guardar los demás, marcados «no está abajo». Para cada uno, lee el archivo mismo en el proyecto, y di cuáles no pudiste revisar.",
            read_all: "Léelos todos antes de juzgar el cambio, y uno largo por partes.",
            read_some: "Lee los que necesites.",
            list_elsewhere: "La lista de archivos cambiados tampoco cabe aquí. Está en {list}, que dice qué diffs van abajo y dónde leer cada uno de los demás: léela primero.",
            heading: "Archivos cambiados:",
            left_out: "no está abajo",
        }
    } else {
        Text {
            too_big: "This change is too big to fit here whole: {files} files, +{added} -{removed} lines. Below are the diffs that fit.",
            others_saved: "The others are marked \"not below\", each with the file that holds it, which you can read but not change.",
            others_not_saved: "Keel couldn't save the others, marked \"not below\". For each, read the file itself in the project, and say which ones you couldn't check.",
            read_all: "Read them all before you judge the change, and a long one in parts.",
            read_some: "Read any of them you need.",
            list_elsewhere: "The list of changed files doesn't fit here either. It's in {list}, which says which diffs are below and where to read each of the others: read it first.",
            heading: "Changed files:",
            left_out: "not below",
        }
    }
}

/// What `{{diff}}` becomes: what Keel says about the change, and the change itself, which goes in
/// a tag of its own. A change that fits goes in whole, and Keel says nothing. Otherwise every
/// file's diff is saved in `folder`, the whole files that fit go in, and each one left out is named
/// with where to read it. `judging` is for an agent that decides on the change, which has to read
/// all of it.
pub fn for_prompt(diff: String, folder: &Path, language: &str, judging: bool) -> (String, String) {
    if diff.len() <= PROMPT_LIMIT {
        return (String::new(), diff);
    }
    let text = text(language);
    let files = split(&diff);
    let names: Vec<PathBuf> = files
        .iter()
        .enumerate()
        .map(|(index, file)| folder.join(file_name(index, &file.path)))
        .collect();
    let line = |file: &FileDiff, at: Option<&Path>| {
        let mut line = format!("- {} +{} -{}", file.path, file.added, file.removed);
        if let Some(at) = at {
            line.push_str(&format!(", {}: {}", text.left_out, at.display()));
        }
        line
    };
    // The list as long as it can be, with every file left out, so what fits doesn't depend on it.
    let longest: usize = files
        .iter()
        .zip(&names)
        .map(|(file, name)| line(file, Some(name)).len() + 1)
        .sum();
    let list_fits = longest <= LIST_LIMIT;
    let mut room = PROMPT_LIMIT.saturating_sub(if list_fits { longest } else { 0 });
    let below: Vec<bool> = files
        .iter()
        .map(|file| {
            let fits = file.text.len() <= room;
            if fits {
                room -= file.text.len();
            }
            fits
        })
        .collect();
    let list: String = files
        .iter()
        .zip(&names)
        .zip(&below)
        .map(|((file, name), below)| line(file, (!below).then_some(name.as_path())) + "\n")
        .collect();
    let saved = save(folder, &files, &names, &format!("{}\n{list}", text.heading)).is_ok();

    let (added, removed) = files
        .iter()
        .fold((0, 0), |(a, r), file| (a + file.added, r + file.removed));
    let mut out = text
        .too_big
        .replace("{files}", &files.len().to_string())
        .replace("{added}", &added.to_string())
        .replace("{removed}", &removed.to_string());
    out.push(' ');
    if saved {
        out.push_str(text.others_saved);
        out.push(' ');
        out.push_str(if judging {
            text.read_all
        } else {
            text.read_some
        });
    } else {
        out.push_str(text.others_not_saved);
    }
    // From here on it's the change: the list of its files, and the diffs that fit.
    let mut change = String::new();
    if list_fits || !saved {
        let list = if saved {
            list
        } else {
            files
                .iter()
                .zip(&below)
                .map(|(file, below)| {
                    let mut line = line(file, None);
                    if !below {
                        line.push_str(&format!(", {}", text.left_out));
                    }
                    line + "\n"
                })
                .collect()
        };
        change.push_str(&format!("{}\n{list}", text.heading));
    } else {
        let at = folder.join(LIST_FILE);
        out.push_str("\n\n");
        out.push_str(
            &text
                .list_elsewhere
                .replace("{list}", &at.display().to_string()),
        );
    }
    for (file, below) in files.iter().zip(&below) {
        if *below {
            if !change.is_empty() {
                change.push('\n');
            }
            change.push_str(&file.text);
        }
    }
    (out, change)
}

/// Saves the change's files to one side and moves them into place, so a folder that's there is
/// whole. Its name says which commit it's the change up to, so a folder already there is the same
/// change and stays as it is.
fn save(folder: &Path, files: &[FileDiff], names: &[PathBuf], list: &str) -> io::Result<()> {
    if folder.join(LIST_FILE).is_file() {
        return Ok(());
    }
    let parent = folder.parent().unwrap_or(folder);
    std::fs::create_dir_all(parent)?;
    let name = folder
        .file_name()
        .map_or_else(|| "change".into(), |name| name.to_string_lossy());
    let partial = parent.join(format!(".{name}.partial"));
    if partial.exists() {
        std::fs::remove_dir_all(&partial)?;
    }
    std::fs::create_dir_all(&partial)?;
    let write = |path: PathBuf, text: &str| -> io::Result<()> {
        use std::io::Write;
        process::create_private(&path)?.write_all(text.as_bytes())
    };
    for (file, name) in files.iter().zip(names) {
        let file_name = name.file_name().unwrap_or_default();
        write(partial.join(file_name), &file.text)?;
    }
    write(partial.join(LIST_FILE), list)?;
    if folder.exists() {
        std::fs::remove_dir_all(folder)?;
    }
    std::fs::rename(&partial, folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(path: &str, body: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n@@ -1,1 +1,2 @@\n x\n{body}"
        )
    }

    fn added(path: &str, lines: usize, width: usize) -> String {
        let body: String = (0..lines)
            .map(|_| format!("+{}\n", "y".repeat(width)))
            .collect();
        part(path, &body)
    }

    #[test]
    fn each_file_of_a_diff_comes_apart_with_its_name_and_lines() {
        let diff = format!(
            "{}{}diff --git a/old.rs b/new.rs\nsimilarity index 100%\nrename from old.rs\nrename to new.rs\n\
             diff --git a/gone.py b/gone.py\ndeleted file mode 100644\n--- a/gone.py\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n\
             diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n\
             diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode 100755\n\
             diff --git \"a/se\\303\\261al.ts\" \"b/se\\303\\261al.ts\"\n--- \"a/se\\303\\261al.ts\"\n+++ \"b/se\\303\\261al.ts\"\n@@ -1 +1 @@\n-x\n+y\n\
             diff --git a/with space.md b/with space.md\n--- a/with space.md\t\n+++ b/with space.md\t\n@@ -1 +1 @@\n-x\n+y\n",
            part("src/a.ts", "+one\n++two, that starts with a plus\n-three\n"),
            part("b.ts", "+x\n"),
        );
        let files = split(&diff);
        let seen: Vec<(&str, usize, usize)> = files
            .iter()
            .map(|f| (f.path.as_str(), f.added, f.removed))
            .collect();
        assert_eq!(
            seen,
            [
                ("src/a.ts", 2, 1),
                ("b.ts", 1, 0),
                ("new.rs", 0, 0),
                ("gone.py", 0, 2),
                ("logo.png", 0, 0),
                ("run.sh", 0, 0),
                ("señal.ts", 1, 1),
                ("with space.md", 1, 1),
            ]
        );
        // Put back together, nothing is missing.
        assert_eq!(
            files.iter().map(|f| f.text.as_str()).collect::<String>(),
            diff
        );
    }

    /// All of it, as it reads in a prompt without the tag the change goes in.
    fn for_prompt(diff: String, folder: &Path, language: &str, judging: bool) -> String {
        let (says, change) = super::for_prompt(diff, folder, language, judging);
        if says.is_empty() {
            change
        } else {
            format!("{says}\n\n{change}")
        }
    }

    #[test]
    fn what_keel_says_about_a_change_too_big_stays_apart_from_the_change() {
        let dir = tempfile::tempdir().unwrap();
        let diff: String = (0..30)
            .map(|i| added(&format!("f{i}.ts"), 100, 100))
            .collect();
        let (says, change) = super::for_prompt(diff.clone(), &dir.path().join("a"), "en", true);
        // What to do about it is Keel's, and goes outside the tag the change is in.
        assert!(says.starts_with("This change is too big to fit here whole: 30 files"));
        assert!(
            says.ends_with("Read them all before you judge the change, and a long one in parts.")
        );
        assert!(!says.contains("diff --git"));
        // The change is its list of files and the diffs that fit, and nothing Keel tells it to do.
        assert!(
            change.starts_with("Changed files:\n- f0.ts +100 -0"),
            "{}",
            &change[..80]
        );
        assert!(change.contains("\ndiff --git "));
        for told in ["too big", "Read them all", "you can read but not change"] {
            assert!(!change.contains(told), "{told}");
        }
        // One that fits has nothing said about it.
        let small = added("a.ts", 3, 10);
        assert_eq!(
            super::for_prompt(small.clone(), &dir.path().join("b"), "en", true),
            (String::new(), small)
        );
    }

    #[test]
    fn a_change_that_fits_goes_in_as_it_is_and_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("change").join("abc");
        let diff = added("a.ts", 3, 10);
        assert_eq!(for_prompt(diff.clone(), &folder, "en", true), diff);
        assert!(!dir.path().join("change").exists());
    }

    /// Every changed file is in the prompt whole, or named with a file that holds all of it.
    fn every_file_is_there(prompt: &str, files: &[FileDiff], left_out: &str) {
        for file in files {
            if prompt.contains(&file.text) {
                continue;
            }
            let line = prompt
                .lines()
                .find(|line| line.starts_with(&format!("- {} ", file.path)))
                .unwrap_or_else(|| panic!("{} isn't listed", file.path));
            let at = line
                .split_once(&format!(", {left_out}: "))
                .unwrap_or_else(|| panic!("{line} says nothing of where it is"))
                .1;
            assert_eq!(std::fs::read_to_string(at).unwrap(), file.text);
        }
    }

    #[test]
    fn a_change_too_big_lists_every_file_and_says_where_each_one_left_out_is() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("change").join("abc");
        // 30 files of about 10 KB, and one bigger than everything that goes in.
        let mut diff: String = (0..30)
            .map(|i| added(&format!("src/f{i}.ts"), 100, 100))
            .collect();
        diff.push_str(&added("huge.json", 3000, 100));
        let files = split(&diff);
        let prompt = for_prompt(diff.clone(), &folder, "en", true);

        assert!(prompt.len() <= PROMPT_LIMIT + 1024, "{}", prompt.len());
        assert!(prompt.starts_with("This change is too big to fit here whole: 31 files"));
        assert!(prompt.contains("Read them all before you judge the change"));
        every_file_is_there(&prompt, &files, "not below");
        // What goes in is only ever whole files.
        for chunk in prompt.split("\ndiff --git ").skip(1) {
            let chunk = format!("diff --git {chunk}");
            let chunk = chunk.strip_suffix('\n').map_or(chunk.as_str(), |c| c);
            assert!(files.iter().any(|f| f.text.trim_end() == chunk.trim_end()));
        }
        assert!(prompt.contains("- huge.json +3000 -0, not below: "));
        // The folder holds every one of them, and the list.
        let saved = std::fs::read_dir(&folder).unwrap().count();
        assert_eq!(saved, files.len() + 1);
        assert!(
            std::fs::read_to_string(folder.join(LIST_FILE))
                .unwrap()
                .contains("huge.json")
        );
    }

    #[test]
    fn whoever_only_learns_from_it_is_not_told_to_read_it_all_and_spanish_says_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let diff: String = (0..30)
            .map(|i| added(&format!("f{i}.ts"), 100, 100))
            .collect();
        let learning = for_prompt(diff.clone(), &dir.path().join("a"), "en", false);
        assert!(learning.contains("Read any of them you need."));
        assert!(!learning.contains("Read them all"));
        let spanish = for_prompt(diff.clone(), &dir.path().join("b"), "es", true);
        assert!(
            spanish
                .starts_with("Este cambio es demasiado grande para caber aquí entero: 30 archivos")
        );
        assert!(spanish.contains("Léelos todos antes de juzgar el cambio"));
        every_file_is_there(&spanish, &split(&diff), "no está abajo");
    }

    #[test]
    fn a_list_too_long_for_the_prompt_is_read_from_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("change").join("abc");
        let diff: String = (0..1000)
            .map(|i| added(&format!("src/a/rather/long/path/to/file{i}.ts"), 2, 50))
            .collect();
        let prompt = for_prompt(diff.clone(), &folder, "en", true);
        let list = folder.join(LIST_FILE);
        assert!(
            prompt.contains(&format!("It's in {}", list.display())),
            "{}",
            &prompt[..600]
        );
        assert!(!prompt.contains("Changed files:"));
        let listed = std::fs::read_to_string(&list).unwrap();
        every_file_is_there(&format!("{listed}{prompt}"), &split(&diff), "not below");
    }

    #[test]
    fn a_folder_already_saved_for_the_same_commit_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("abc");
        let diff: String = (0..30)
            .map(|i| added(&format!("f{i}.ts"), 100, 100))
            .collect();
        let first = for_prompt(diff.clone(), &folder, "en", true);
        let second = for_prompt(diff, &folder, "en", false);
        assert_eq!(
            first.lines().skip(1).collect::<Vec<_>>(),
            second.lines().skip(1).collect::<Vec<_>>()
        );
        assert!(!dir.path().join(".abc.partial").exists());
    }

    #[test]
    fn when_nothing_can_be_saved_the_prompt_still_names_what_it_leaves_out() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the folder would go.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "").unwrap();
        let diff: String = (0..30)
            .map(|i| added(&format!("f{i}.ts"), 100, 100))
            .collect();
        let prompt = for_prompt(diff.clone(), &blocked.join("abc"), "en", true);
        assert!(prompt.contains("Keel couldn't save the others, marked \"not below\"."));
        for file in split(&diff) {
            assert!(
                prompt.contains(&file.text)
                    || prompt.contains(&format!("- {} +100 -0, not below\n", file.path)),
                "{}",
                file.path
            );
        }
    }

    #[test]
    fn names_never_reach_outside_the_folder() {
        assert_eq!(file_name(0, "src/a.ts"), "001-a.ts.diff");
        assert_eq!(file_name(41, "../../etc/pass wd"), "042-pass_wd.diff");
        assert_eq!(file_name(2, "\"a/b\""), "003-b_.diff");
        assert_eq!(file_name(3, ".."), "004-...diff");
    }
}
