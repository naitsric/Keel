//! Builders kept changing source files from Bash, with `cat >`, `sed -i` or a Python script, even
//! with their prompt asking for Edit and Write. A replacement in a script that matches nothing
//! changes nothing and says nothing; Edit fails out loud. Claude Code asks Keel about each Bash
//! command of a step that can edit, through a `PreToolUse` hook, and this is what answers.
//!
//! It reads a command the way a shell would, well enough to see where output goes. Whatever it
//! can't follow goes through: a command turned back by mistake costs the agent a turn, and one let
//! through by mistake costs what every run cost before.

use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// The first argument that makes the Keel app answer as the hook instead of opening.
pub const HOOK_ARG: &str = "bash-edit-hook";

/// What Claude Code shows the agent when a command is turned back.
fn message(what: &str) -> String {
    format!(
        "Keel stopped this command: {what}. Change files with Edit or Write, not from Bash: Edit \
         fails when the text does not match, while sed or a script can change nothing and say \
         nothing. Use Bash to run commands, and send output you want to keep to $TMPDIR."
    )
}

/// The hook, given the JSON Claude Code sends on stdin. `Err` holds what to tell the agent.
/// Anything that isn't a command Keel can read goes through.
pub fn hook(workspace: &Path, input: &str) -> Result<(), String> {
    let Ok(event) = serde_json::from_str::<Value>(input) else {
        return Ok(());
    };
    let Some(command) = event.pointer("/tool_input/command").and_then(Value::as_str) else {
        return Ok(());
    };
    if !workspace.is_absolute() {
        return Ok(());
    }
    let cwd = event
        .get("cwd")
        .and_then(Value::as_str)
        .map_or_else(|| workspace.to_path_buf(), PathBuf::from);
    let found = workspace_write(command, &cwd, workspace).or_else(|| {
        // Claude Code may report the real path of a folder Keel named through a link.
        let real = |path: &Path| std::fs::canonicalize(path).ok();
        workspace_write(command, &real(&cwd)?, &real(workspace)?)
    });
    match found {
        Some(what) => Err(message(&what)),
        None => Ok(()),
    }
}

/// What the app's binary does when Claude Code runs it as the hook: exit code 2 turns the command
/// back with stderr as the reason, 0 lets it run.
pub fn run_hook(workspace: &Path) -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0;
    }
    match hook(workspace, &input) {
        Ok(()) => 0,
        Err(reason) => {
            eprintln!("{reason}");
            2
        }
    }
}

/// Whether `command`, run in `cwd`, writes to a file inside `workspace` in a way that belongs to
/// Edit and Write: a redirection or `tee` into it, `sed -i` or `perl -i` on it, or a Python or
/// Node script that writes files. Returns what it does, for the agent; `None` when it doesn't, or
/// when that can't be told.
pub fn workspace_write(command: &str, cwd: &Path, workspace: &Path) -> Option<String> {
    check_text(command, Some(cwd.to_path_buf()), workspace, 0)
}

fn check_text(
    command: &str,
    cwd: Option<PathBuf>,
    workspace: &Path,
    depth: usize,
) -> Option<String> {
    if depth > 3 {
        return None;
    }
    let items = parse(command)?;
    // `cd` moves where relative paths point, until the subshell it ran in ends.
    let mut dirs = vec![cwd];
    let mut previous: Option<&Simple> = None;
    for item in &items {
        match item {
            Item::Enter => dirs.push(dirs.last().cloned().flatten()),
            Item::Leave => {
                if dirs.len() > 1 {
                    dirs.pop();
                }
            }
            Item::Command(simple) => {
                let here = dirs.last().cloned().flatten();
                let place = Place {
                    cwd: here.as_deref(),
                    workspace,
                };
                let fed = previous.filter(|_| simple.piped);
                if let Some(found) = place.check(simple, fed, depth) {
                    return Some(found);
                }
                if let Some(next) = place.cd(simple)
                    && let Some(last) = dirs.last_mut()
                {
                    *last = next;
                }
                previous = Some(simple);
            }
        }
    }
    None
}

/// Shell words that come before the command itself.
const KEYWORDS: [&str; 11] = [
    "!", "{", "}", "if", "then", "else", "elif", "do", "while", "until", "time",
];

/// Things a script does when it writes a file: `open` in a mode that writes, pathlib's writers,
/// `fileinput` in place, and Node's `fs` writers.
static WRITES_FILES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"\bopen\s*\((?:[^()]|\([^()]*\))*?,\s*(?:mode\s*=\s*)?['"][rbt]*[wax+][rwxabt+]*['"]"#,
        r#"|\.open\s*\(\s*(?:mode\s*=\s*)?['"][rbt]*[wax+][rwxabt+]*['"]"#,
        r#"|\.write_(?:text|bytes)\s*\("#,
        r#"|\binplace\s*=\s*(?:True|1)\b"#,
        r#"|\b(?:writeFileSync|writeFile|appendFileSync|appendFile|createWriteStream)\s*\("#,
    ))
    .expect("a valid pattern")
});

/// The sandbox lets a command write only to the project and to temporary folders, so a script
/// that names none of these writes to the project.
static NAMES_TEMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"tempfile|mkdtemp|mkstemp|gettempdir|TMPDIR|tmpdir\s*\(|/tmp\b|/var/folders/|/dev/(?:null|std)")
        .expect("a valid pattern")
});

fn writes_files(source: &str) -> bool {
    WRITES_FILES.is_match(source)
}

/// A script saved to run later, the way builders wrote `$TMPDIR/fix.py` and then ran it.
fn saves_editing_script(path: &str, body: &str) -> bool {
    is_script_name(path) && writes_files(body) && !NAMES_TEMP.is_match(body)
}

fn is_script_name(path: &str) -> bool {
    [".py", ".js", ".mjs", ".cjs"]
        .iter()
        .any(|extension| path.ends_with(extension))
}

fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_python(name: &str) -> bool {
    name.strip_prefix("python")
        .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
}

fn is_assignment(word: &Word) -> bool {
    match word.text.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Skips options up to the first other word. `with_value` lists the ones that take the next word.
fn skip_options<'w>(words: &'w [Word], with_value: &[&str]) -> &'w [Word] {
    let mut rest = words;
    while let Some((first, tail)) = rest.split_first() {
        let text = first.text.as_str();
        if text == "--" {
            return tail;
        }
        if !text.starts_with('-') || text == "-" {
            break;
        }
        rest = if with_value.contains(&text) {
            tail.get(1..).unwrap_or(&[])
        } else {
            tail
        };
    }
    rest
}

/// The command that actually runs, past keywords, variable assignments and wrappers like `env`,
/// and whether `xargs` hands it its files.
fn command_words(words: &[Word]) -> (&[Word], bool) {
    let mut rest = words;
    let mut through_xargs = false;
    while let Some((first, tail)) = rest.split_first() {
        if (!first.quoted && KEYWORDS.contains(&first.text.as_str())) || is_assignment(first) {
            rest = tail;
            continue;
        }
        rest = match program(&first.text) {
            "env" | "command" | "builtin" | "exec" | "nohup" | "sudo" => {
                skip_options(tail, &["-u", "-C", "-S", "-g"])
            }
            "nice" => skip_options(tail, &["-n"]),
            "timeout" | "gtimeout" => skip_options(tail, &["-s", "-k"]).get(1..).unwrap_or(&[]),
            "xargs" => {
                through_xargs = true;
                skip_options(
                    tail,
                    &[
                        "-I", "-n", "-P", "-L", "-d", "-E", "-s", "-a", "-J", "-R", "-S",
                    ],
                )
            }
            _ => break,
        };
    }
    (rest, through_xargs)
}

/// What a command reads on stdin, when it's text in the command itself: a here-document, a
/// here-string, or what `cat`, `echo` or `printf` pipe into it.
fn stdin_text(simple: &Simple, fed: Option<&Simple>) -> Option<String> {
    if !simple.stdin.is_empty() {
        return Some(simple.stdin.join("\n"));
    }
    let fed = fed?;
    let (words, _) = command_words(&fed.words);
    let (first, args) = words.split_first()?;
    match program(&first.text) {
        "cat" if args.is_empty() && !fed.stdin.is_empty() => Some(fed.stdin.join("\n")),
        "echo" | "printf" => Some(
            args.iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// The program Python runs, when it's in the command rather than in a file.
fn python_source(args: &[Word], simple: &Simple, fed: Option<&Simple>) -> Option<String> {
    let mut rest = args;
    loop {
        let Some((first, tail)) = rest.split_first() else {
            return stdin_text(simple, fed);
        };
        let text = first.text.as_str();
        match text {
            "-c" => return tail.first().map(|word| word.text.clone()),
            "-" => return stdin_text(simple, fed),
            "-m" => return None,
            "-W" | "-X" => rest = tail.get(1..).unwrap_or(&[]),
            _ if text.starts_with("-c") => return Some(text[2..].to_owned()),
            _ if text.starts_with('-') => rest = tail,
            _ => return None,
        }
    }
}

/// The program Node runs, when it's in the command rather than in a file.
fn node_source(args: &[Word], simple: &Simple, fed: Option<&Simple>) -> Option<String> {
    let mut rest = args;
    loop {
        let Some((first, tail)) = rest.split_first() else {
            return stdin_text(simple, fed);
        };
        let text = first.text.as_str();
        match text {
            "-e" | "--eval" | "-p" | "--print" => {
                return tail.first().map(|word| word.text.clone());
            }
            "-" => return stdin_text(simple, fed),
            "-r" | "--require" | "--import" | "--loader" | "-C" | "--conditions" => {
                rest = tail.get(1..).unwrap_or(&[]);
            }
            _ if text.starts_with("--eval=") || text.starts_with("--print=") => {
                return text.split_once('=').map(|(_, code)| code.to_owned());
            }
            _ if text.starts_with('-') => rest = tail,
            _ => return None,
        }
    }
}

/// The files `sed` edits in place, or `None` when it doesn't edit in place. GNU writes `-i` with
/// an optional suffix in the same word; macOS takes the suffix as the next word, as in `-i ''`.
fn sed_in_place(args: &[Word]) -> Option<Vec<&Word>> {
    let mut in_place = false;
    let mut script_given = false;
    let mut operands = Vec::new();
    let mut only_operands = false;
    let mut at = 0;
    while let Some(word) = args.get(at) {
        at += 1;
        let text = word.text.as_str();
        if only_operands || !text.starts_with('-') || text == "-" {
            operands.push(word);
            continue;
        }
        if text == "--" {
            only_operands = true;
            continue;
        }
        if let Some(long) = text.strip_prefix("--") {
            if long == "in-place" || long.starts_with("in-place=") {
                in_place = true;
            } else if long == "expression" || long == "file" {
                script_given = true;
                at += 1;
            } else if long.starts_with("expression=") || long.starts_with("file=") {
                script_given = true;
            } else if long == "line-length" {
                at += 1;
            }
            continue;
        }
        let flags: Vec<char> = text[1..].chars().collect();
        for (index, flag) in flags.iter().enumerate() {
            let last = index + 1 == flags.len();
            match flag {
                'i' | 'I' => {
                    in_place = true;
                    if last
                        && args
                            .get(at)
                            .is_some_and(|next| next.text.is_empty() || next.text.starts_with('.'))
                    {
                        at += 1;
                    }
                    break;
                }
                'e' | 'f' => {
                    script_given = true;
                    if last {
                        at += 1;
                    }
                    break;
                }
                'l' => {
                    if last {
                        at += 1;
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    // Without `-e` or `-f`, the first word that isn't an option is the script.
    let skip = usize::from(!script_given);
    in_place.then(|| operands.into_iter().skip(skip).collect())
}

/// The files `perl -i` edits in place, or `None` when it doesn't edit in place. Switches cluster,
/// as in `-pi`, and some take the rest of the word, as in `-Ilib`.
fn perl_in_place(args: &[Word]) -> Option<Vec<&Word>> {
    let mut in_place = false;
    let mut code_given = false;
    let mut operands = Vec::new();
    let mut only_operands = false;
    let mut at = 0;
    while let Some(word) = args.get(at) {
        at += 1;
        let text = word.text.as_str();
        if only_operands || !text.starts_with('-') || text == "-" {
            operands.push(word);
            continue;
        }
        if text == "--" {
            only_operands = true;
            continue;
        }
        let flags: Vec<char> = text[1..].chars().collect();
        let mut index = 0;
        while let Some(&flag) = flags.get(index) {
            index += 1;
            let last = index == flags.len();
            match flag {
                'i' => {
                    in_place = true;
                    break;
                }
                'e' | 'E' => {
                    code_given = true;
                    if last {
                        at += 1;
                    }
                    break;
                }
                'I' => {
                    if last {
                        at += 1;
                    }
                    break;
                }
                'M' | 'm' | 'd' | 'D' | 'C' | 'F' | 'x' => break,
                'l' | '0' => {
                    while flags.get(index).is_some_and(char::is_ascii_digit) {
                        index += 1;
                    }
                }
                _ => {}
            }
        }
    }
    let skip = usize::from(!code_given);
    in_place.then(|| operands.into_iter().skip(skip).collect())
}

/// Where a command runs, to tell whether a path it names is inside the project.
struct Place<'a> {
    /// `None` once a `cd` went somewhere that can't be told.
    cwd: Option<&'a Path>,
    workspace: &'a Path,
}

impl Place<'_> {
    /// The path a word names, when it can be told without running anything.
    fn resolve(&self, word: &Word) -> Option<PathBuf> {
        let text = word.text.as_str();
        if word.dynamic {
            // Only these are known here. `$TMPDIR` and `$HOME` are outside the project anyway.
            let strip = |name: &str| {
                [format!("${name}"), format!("${{{name}}}")]
                    .iter()
                    .find_map(|prefix| text.strip_prefix(prefix.as_str()))
                    .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                    .filter(|rest| !rest.contains(['$', '`']))
                    .map(|rest| rest.trim_start_matches('/').to_owned())
            };
            if let Some(rest) = strip("PWD") {
                return Some(normalize(&self.cwd?.join(rest)));
            }
            if let Some(rest) = strip("CLAUDE_PROJECT_DIR") {
                return Some(normalize(&self.workspace.join(rest)));
            }
            return None;
        }
        if text.is_empty() || text.starts_with('~') {
            return None;
        }
        if text.starts_with('/') {
            return Some(normalize(Path::new(text)));
        }
        Some(normalize(&self.cwd?.join(text)))
    }

    /// The path inside the project a word names, if it names one. Git's own files aren't the
    /// project's: an agent may write a commit message there.
    fn in_project(&self, word: &Word) -> Option<String> {
        let path = self.resolve(word)?;
        let inside = path.strip_prefix(self.workspace).ok()?;
        if inside.starts_with(".git") {
            return None;
        }
        let shown = inside.to_string_lossy();
        Some(if shown.is_empty() {
            ".".to_owned()
        } else {
            shown.into_owned()
        })
    }

    fn cwd_in_project(&self) -> bool {
        self.cwd.is_some_and(|cwd| cwd.starts_with(self.workspace))
    }

    /// Where a `cd` leaves the commands after it, or `None` when this isn't one.
    fn cd(&self, simple: &Simple) -> Option<Option<PathBuf>> {
        let (words, _) = command_words(&simple.words);
        let (first, args) = words.split_first()?;
        match first.text.as_str() {
            "cd" | "pushd" => {}
            "popd" => return Some(None),
            _ => return None,
        }
        // No folder means home, and `-` the one before: neither is known here.
        let target = skip_options(args, &[]).first();
        Some(
            target
                .filter(|word| word.text != "-")
                .and_then(|word| self.resolve(word)),
        )
    }

    fn check(&self, simple: &Simple, fed: Option<&Simple>, depth: usize) -> Option<String> {
        for (operator, target) in &simple.writes {
            // `2>&1` and `>&-` copy or close a descriptor. Only `>&file` writes.
            if operator == ">&"
                && (target.text == "-" || target.text.chars().all(|c| c.is_ascii_digit()))
            {
                continue;
            }
            if let Some(path) = self.in_project(target) {
                return Some(format!("it writes {path} with {operator}"));
            }
        }
        if simple.writes.iter().any(|(_, target)| {
            simple
                .stdin
                .iter()
                .any(|body| saves_editing_script(&target.text, body))
        }) {
            return Some("it saves a script that writes files".to_owned());
        }
        let (words, through_xargs) = command_words(&simple.words);
        self.check_words(words, through_xargs, simple, fed, depth)
    }

    fn check_words(
        &self,
        words: &[Word],
        through_xargs: bool,
        simple: &Simple,
        fed: Option<&Simple>,
        depth: usize,
    ) -> Option<String> {
        let (first, args) = words.split_first()?;
        let name = program(&first.text);
        match name {
            "tee" => {
                let files: Vec<&Word> = skip_options(args, &[]).iter().collect();
                if let Some(path) = files.iter().find_map(|file| self.in_project(file)) {
                    return Some(format!("it writes {path} with tee"));
                }
                let body = stdin_text(simple, fed)?;
                files
                    .iter()
                    .any(|file| saves_editing_script(&file.text, &body))
                    .then(|| "it saves a script that writes files".to_owned())
            }
            "sed" | "gsed" => self.in_place("sed -i", sed_in_place(args)?, through_xargs),
            "perl" => self.in_place("perl -i", perl_in_place(args)?, through_xargs),
            "sh" | "bash" | "zsh" | "dash" => {
                let script = match args.iter().position(|word| {
                    word.text.starts_with('-')
                        && !word.text.starts_with("--")
                        && word.text.contains('c')
                }) {
                    Some(at) => args.get(at + 1)?.text.clone(),
                    None if skip_options(args, &[]).is_empty() => stdin_text(simple, fed)?,
                    None => return None,
                };
                check_text(
                    &script,
                    self.cwd.map(Path::to_path_buf),
                    self.workspace,
                    depth + 1,
                )
            }
            "eval" => {
                let script = args
                    .iter()
                    .map(|word| word.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                check_text(
                    &script,
                    self.cwd.map(Path::to_path_buf),
                    self.workspace,
                    depth + 1,
                )
            }
            "find" => self.find_exec(args, depth),
            "node" | "nodejs" => self.script("Node", node_source(args, simple, fed)?, words),
            _ if is_python(name) => self.script("Python", python_source(args, simple, fed)?, words),
            _ => None,
        }
    }

    fn in_place(&self, tool: &str, files: Vec<&Word>, through_xargs: bool) -> Option<String> {
        // Files that come through xargs are listed by a command run where the agent works.
        if files.is_empty() && through_xargs && self.cwd_in_project() {
            return Some(format!("{tool} changes the files xargs hands it"));
        }
        files
            .iter()
            .find_map(|file| self.in_project(file))
            .map(|path| format!("{tool} changes {path}"))
    }

    fn script(&self, language: &str, source: String, words: &[Word]) -> Option<String> {
        let mentioned = words
            .iter()
            .map(|word| word.text.as_str())
            .chain([source.as_str()])
            .any(|text| NAMES_TEMP.is_match(text));
        (writes_files(&source) && !mentioned && self.cwd_in_project())
            .then(|| format!("its {language} script writes files"))
    }

    /// `find … -exec sed -i … {} +` runs a command on each file it finds, under the folders it
    /// starts from.
    fn find_exec(&self, args: &[Word], depth: usize) -> Option<String> {
        let roots: Vec<&Word> = args
            .iter()
            .take_while(|word| !word.text.starts_with(['-', '(', '!']))
            .collect();
        let root = roots.first().map_or_else(
            || Word {
                text: ".".to_owned(),
                ..Word::default()
            },
            |root| (*root).clone(),
        );
        let mut rest = args;
        while let Some(at) = rest
            .iter()
            .position(|word| matches!(word.text.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
        {
            let tail = &rest[at + 1..];
            let end = tail
                .iter()
                .position(|word| word.text == ";" || word.text == "+")
                .unwrap_or(tail.len());
            let command: Vec<Word> = tail[..end]
                .iter()
                .map(|word| {
                    if word.text == "{}" {
                        root.clone()
                    } else {
                        word.clone()
                    }
                })
                .collect();
            let found = self.check_words(&command, false, &Simple::default(), None, depth);
            if found.is_some() {
                return found;
            }
            rest = tail.get(end + 1..).unwrap_or(&[]);
        }
        None
    }
}

/// `..` and `.` taken out, without asking the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[derive(Debug, Default, Clone)]
struct Word {
    /// With quotes taken out. Expansions stay as written.
    text: String,
    /// It has an expansion, `$NAME`, `$(…)` or backticks, so its value is known only when it runs.
    dynamic: bool,
    quoted: bool,
}

/// One command with its words and redirections.
#[derive(Debug, Default, Clone)]
struct Simple {
    words: Vec<Word>,
    /// Where its output goes: each operator and its target.
    writes: Vec<(String, Word)>,
    /// Its here-documents and here-strings.
    stdin: Vec<String>,
    /// Its stdin is the output of the command before it.
    piped: bool,
}

enum Item {
    Command(Simple),
    /// A subshell starts or ends. A `cd` inside it doesn't last past it.
    Enter,
    Leave,
}

/// The command being read.
#[derive(Default)]
struct Current {
    command: Simple,
    word: Word,
    in_word: bool,
    /// An operator waiting for its target, like `>` before a file name.
    redirect: Option<String>,
    /// Delimiters of here-documents whose bodies start on the next line, and whether their
    /// leading tabs go (`<<-`).
    heredocs: Vec<(String, bool)>,
    /// Inside `[[ … ]]`, where `<` and `>` compare strings.
    test: bool,
}

/// The commands in a line of shell, in the order they run, or `None` when it can't be read.
fn parse(command: &str) -> Option<Vec<Item>> {
    let mut parser = Parser::new(command);
    parser.list(false, 0)?;
    Some(parser.items)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    items: Vec<Item>,
    /// Here-documents waiting for their bodies: the command they belong to, the delimiter, and
    /// whether leading tabs go.
    heredocs: Vec<(usize, String, bool)>,
}

impl Parser {
    fn new(text: &str) -> Self {
        Self {
            chars: text.chars().collect(),
            pos: 0,
            items: Vec::new(),
            heredocs: Vec::new(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        Some(c)
    }

    /// Reads commands to the end, or, when `nested`, to the `)` that closes a subshell.
    fn list(&mut self, nested: bool, depth: usize) -> Option<()> {
        if depth > 16 {
            return None;
        }
        let mut cur = Current::default();
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' => {
                    self.pos += 1;
                    self.end_word(&mut cur);
                }
                '\n' => {
                    self.pos += 1;
                    self.end_command(&mut cur)?;
                    self.read_heredocs();
                }
                '#' if !cur.in_word => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                ';' => {
                    self.pos += 1;
                    self.end_command(&mut cur)?;
                }
                '&' => {
                    self.pos += 1;
                    if self.peek() == Some('>') {
                        // `&>` and `&>>` send stdout and stderr together.
                        self.pos += 1;
                        self.end_word(&mut cur);
                        if cur.redirect.is_some() {
                            return None;
                        }
                        let operator = if self.peek() == Some('>') {
                            self.pos += 1;
                            "&>>"
                        } else {
                            "&>"
                        };
                        cur.redirect = Some(operator.to_owned());
                    } else {
                        if self.peek() == Some('&') {
                            self.pos += 1;
                        }
                        self.end_command(&mut cur)?;
                    }
                }
                '|' => {
                    self.pos += 1;
                    let pipe = match self.peek() {
                        Some('|') => {
                            self.pos += 1;
                            false
                        }
                        Some('&') => {
                            self.pos += 1;
                            true
                        }
                        _ => true,
                    };
                    self.end_command(&mut cur)?;
                    cur.command.piped = pipe;
                }
                '(' if !cur.in_word => {
                    if self.peek_at(1) == Some('(') {
                        // Arithmetic, where `<` and `>` compare numbers.
                        let text = self.arithmetic()?;
                        cur.in_word = true;
                        cur.word.dynamic = true;
                        cur.word.text.push_str(&text);
                    } else {
                        self.pos += 1;
                        self.end_command(&mut cur)?;
                        self.subshell(depth)?;
                    }
                }
                ')' => {
                    self.pos += 1;
                    self.end_command(&mut cur)?;
                    if nested {
                        return Some(());
                    }
                }
                '<' | '>' if !cur.test => self.redirection(&mut cur, depth)?,
                _ => self.word_char(&mut cur, depth)?,
            }
        }
        self.end_command(&mut cur)?;
        (!nested).then_some(())
    }

    fn subshell(&mut self, depth: usize) -> Option<()> {
        self.items.push(Item::Enter);
        self.list(true, depth + 1)?;
        self.items.push(Item::Leave);
        Some(())
    }

    fn end_word(&mut self, cur: &mut Current) {
        if !cur.in_word {
            return;
        }
        cur.in_word = false;
        let word = std::mem::take(&mut cur.word);
        match cur.redirect.take() {
            Some(operator) if operator == "<<" || operator == "<<-" => {
                cur.heredocs.push((word.text, operator == "<<-"));
            }
            Some(operator) if operator == "<<<" => cur.command.stdin.push(word.text),
            Some(operator) if operator == "<" || operator == "<&" => {}
            Some(operator) => cur.command.writes.push((operator, word)),
            None => {
                if !word.quoted && word.text == "[[" {
                    cur.test = true;
                }
                if !word.quoted && word.text == "]]" {
                    cur.test = false;
                }
                cur.command.words.push(word);
            }
        }
    }

    fn end_command(&mut self, cur: &mut Current) -> Option<()> {
        self.end_word(cur);
        // An operator with nothing after it is a syntax error.
        if cur.redirect.is_some() {
            return None;
        }
        cur.test = false;
        let command = &cur.command;
        if command.words.is_empty()
            && command.writes.is_empty()
            && command.stdin.is_empty()
            && cur.heredocs.is_empty()
        {
            return Some(());
        }
        let index = self.items.len();
        self.items
            .push(Item::Command(std::mem::take(&mut cur.command)));
        for (delimiter, strip) in std::mem::take(&mut cur.heredocs) {
            self.heredocs.push((index, delimiter, strip));
        }
        Some(())
    }

    /// Here-document bodies start on the line after the command, in the order they were opened.
    fn read_heredocs(&mut self) {
        for (index, delimiter, strip) in std::mem::take(&mut self.heredocs) {
            let mut body = String::new();
            while self.pos < self.chars.len() {
                let start = self.pos;
                while self.peek().is_some_and(|c| c != '\n') {
                    self.pos += 1;
                }
                let raw: String = self.chars[start..self.pos].iter().collect();
                if self.peek() == Some('\n') {
                    self.pos += 1;
                }
                let line = if strip {
                    raw.trim_start_matches('\t')
                } else {
                    raw.as_str()
                };
                if line == delimiter {
                    break;
                }
                body.push_str(line);
                body.push('\n');
            }
            if let Some(Item::Command(command)) = self.items.get_mut(index) {
                command.stdin.push(body);
            }
        }
    }

    fn redirection(&mut self, cur: &mut Current, depth: usize) -> Option<()> {
        // Process substitution: a command whose output or input stands in for a file.
        if self.peek_at(1) == Some('(') {
            self.end_word(cur);
            self.pos += 2;
            self.subshell(depth)?;
            cur.in_word = true;
            cur.word.dynamic = true;
            cur.word.text.push_str("<(…)");
            return Some(());
        }
        // Digits right before the operator name a descriptor, as in `2>`, not a word.
        if cur.in_word
            && !cur.word.quoted
            && !cur.word.dynamic
            && cur.word.text.chars().all(|c| c.is_ascii_digit())
        {
            cur.word = Word::default();
            cur.in_word = false;
        } else {
            self.end_word(cur);
        }
        if cur.redirect.is_some() {
            return None;
        }
        let c = self.next()?;
        let operator = match (c, self.peek()) {
            ('>', Some(next @ ('>' | '|' | '&'))) => {
                self.pos += 1;
                match next {
                    '>' => ">>",
                    '|' => ">|",
                    _ => ">&",
                }
            }
            ('>', _) => ">",
            ('<', Some('<')) => {
                self.pos += 1;
                match self.peek() {
                    Some('<') => {
                        self.pos += 1;
                        "<<<"
                    }
                    Some('-') => {
                        self.pos += 1;
                        "<<-"
                    }
                    _ => "<<",
                }
            }
            ('<', Some('&')) => {
                self.pos += 1;
                "<&"
            }
            ('<', Some('>')) => {
                self.pos += 1;
                "<>"
            }
            _ => "<",
        };
        cur.redirect = Some(operator.to_owned());
        Some(())
    }

    fn word_char(&mut self, cur: &mut Current, depth: usize) -> Option<()> {
        let c = self.next()?;
        cur.in_word = true;
        match c {
            '\'' => {
                cur.word.quoted = true;
                loop {
                    match self.next()? {
                        '\'' => break,
                        other => cur.word.text.push(other),
                    }
                }
            }
            '"' => {
                cur.word.quoted = true;
                loop {
                    match self.next()? {
                        '"' => break,
                        '\\' => match self.next()? {
                            escaped @ ('$' | '`' | '"' | '\\') => cur.word.text.push(escaped),
                            '\n' => {}
                            other => {
                                cur.word.text.push('\\');
                                cur.word.text.push(other);
                            }
                        },
                        '$' => self.dollar(cur, depth)?,
                        '`' => self.backtick(cur, depth)?,
                        other => cur.word.text.push(other),
                    }
                }
            }
            '\\' => match self.next() {
                // A line continuation joins lines and is no word of its own.
                Some('\n') => cur.in_word = !cur.word.text.is_empty() || cur.word.quoted,
                Some(escaped) => {
                    cur.word.quoted = true;
                    cur.word.text.push(escaped);
                }
                None => {}
            },
            '$' => self.dollar(cur, depth)?,
            '`' => self.backtick(cur, depth)?,
            other => cur.word.text.push(other),
        }
        Some(())
    }

    /// After a `$`.
    fn dollar(&mut self, cur: &mut Current, depth: usize) -> Option<()> {
        match self.peek() {
            Some('(') if self.peek_at(1) == Some('(') => {
                let text = self.arithmetic()?;
                cur.word.text.push('$');
                cur.word.text.push_str(&text);
                cur.word.dynamic = true;
            }
            Some('(') => {
                self.pos += 1;
                self.subshell(depth)?;
                cur.word.text.push_str("$(…)");
                cur.word.dynamic = true;
            }
            Some('{') => {
                let start = self.pos;
                let mut level = 0usize;
                loop {
                    match self.next()? {
                        '{' => level += 1,
                        '}' => {
                            level -= 1;
                            if level == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                cur.word.text.push('$');
                cur.word.text.extend(&self.chars[start..self.pos]);
                cur.word.dynamic = true;
            }
            Some('\'') => {
                // `$'…'` quotes with backslash escapes.
                self.pos += 1;
                cur.word.quoted = true;
                loop {
                    match self.next()? {
                        '\'' => break,
                        '\\' => cur.word.text.push(match self.next()? {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        }),
                        other => cur.word.text.push(other),
                    }
                }
            }
            Some(c) if c.is_ascii_alphanumeric() || c == '_' => {
                cur.word.text.push('$');
                while let Some(c) = self
                    .peek()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    cur.word.text.push(c);
                    self.pos += 1;
                }
                cur.word.dynamic = true;
            }
            Some(c) if "?#@*!$-".contains(c) => {
                self.pos += 1;
                cur.word.text.push('$');
                cur.word.text.push(c);
                cur.word.dynamic = true;
            }
            _ => cur.word.text.push('$'),
        }
        Some(())
    }

    /// `((…))`, read whole: `<` and `>` in it compare numbers.
    fn arithmetic(&mut self) -> Option<String> {
        let start = self.pos;
        let mut level = 0usize;
        loop {
            match self.next()? {
                '(' => level += 1,
                ')' => {
                    level -= 1;
                    if level == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        Some(self.chars[start..self.pos].iter().collect())
    }

    /// After an opening backtick. What's inside runs as well, so it's read as commands too.
    fn backtick(&mut self, cur: &mut Current, depth: usize) -> Option<()> {
        let mut inner = String::new();
        loop {
            match self.next()? {
                '`' => break,
                '\\' => {
                    let escaped = self.next()?;
                    if !matches!(escaped, '`' | '\\' | '$') {
                        inner.push('\\');
                    }
                    inner.push(escaped);
                }
                other => inner.push(other),
            }
        }
        let mut sub = Parser::new(&inner);
        sub.list(false, depth + 1)?;
        self.items.push(Item::Enter);
        self.items.extend(sub.items);
        self.items.push(Item::Leave);
        cur.word.text.push_str("`…`");
        cur.word.dynamic = true;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "/runs/r1/workspace";

    fn writes(command: &str) -> Option<String> {
        workspace_write(command, Path::new(WORKSPACE), Path::new(WORKSPACE))
    }

    fn assert_blocked(commands: &[&str]) {
        for command in commands {
            assert!(writes(command).is_some(), "should be blocked: {command}");
        }
    }

    fn assert_allowed(commands: &[&str]) {
        for command in commands {
            assert_eq!(writes(command), None, "should be allowed: {command}");
        }
    }

    #[test]
    fn writing_a_project_file_through_a_redirection_is_blocked() {
        assert_blocked(&[
            "cat > src/lib.rs <<'EOF'\nfn main() {}\nEOF",
            "cat <<'EOF' > src/lib.rs\nfn main() {}\nEOF",
            "echo 'x' >> README.md",
            "printf 'a\\n' > src/a.txt",
            "echo x>src/a.txt",
            ": > src/empty.rs",
            "cargo test > test.log 2>&1",
            "cargo test 2> errors.txt",
            "cargo test &> all.txt",
            "{ echo a; echo b; } > src/list.txt",
            "for f in a b; do echo $f; done > names.txt",
            "if true; then echo x > src/a.rs; fi",
            "git diff >| changes.patch",
        ]);
        assert_eq!(
            writes("cat > src/lib.rs <<'EOF'\nfn main() {}\nEOF").as_deref(),
            Some("it writes src/lib.rs with >")
        );
    }

    #[test]
    fn tee_into_the_project_is_blocked() {
        assert_blocked(&[
            "cat <<'EOF' | tee src/a.rs\nfn a() {}\nEOF",
            "echo x | tee -a notes.md",
            "cargo test 2>&1 | tee test.log",
        ]);
    }

    #[test]
    fn sed_and_perl_in_place_are_blocked_in_every_spelling() {
        assert_blocked(&[
            "sed -i '' 's/a/b/' src/lib.rs",
            "sed -i 's/a/b/' src/lib.rs",
            "sed -i.bak -e 's/a/b/' src/lib.rs",
            "sed -i -e 's/a/b/' src/lib.rs",
            "sed -Ei '' 's/a+/b/' src/lib.rs",
            "sed --in-place 's/a/b/' src/lib.rs",
            "sed 's/a/b/' -i src/lib.rs",
            "LC_ALL=C sed -i '' 's/a/b/' src/lib.rs",
            "find src -name '*.rs' -exec sed -i '' 's/a/b/' {} +",
            "grep -rl foo src | xargs sed -i '' 's/foo/bar/g'",
            "perl -pi -e 's/a/b/' src/lib.rs",
            "perl -i -pe 's/a/b/' src/lib.rs",
            "perl -0777 -i.bak -pe 's/a/b/g' src/lib.rs",
            "bash -c \"sed -i '' 's/a/b/' src/lib.rs\"",
        ]);
        assert_eq!(
            writes("sed -i '' 's/a/b/' src/lib.rs").as_deref(),
            Some("sed -i changes src/lib.rs")
        );
    }

    #[test]
    fn a_python_or_node_script_that_writes_files_is_blocked() {
        assert_blocked(&[
            // What the builder in run 40c0fe47 did.
            "python3 - <<'EOF'\np = 'crates/keel-engine/src/brain.rs'\ns = open(p).read()\nopen(p, 'w').write(s.replace('a', 'b'))\nEOF",
            "python3 <<'EOF'\nwith open(path, \"w\", encoding=\"utf-8\") as f:\n    f.write(text)\nEOF",
            "python3 -c \"from pathlib import Path; Path('a.rs').write_text('x')\"",
            "python -c 'import io; io.open(\"a.txt\", mode=\"a\").write(\"y\")'",
            "python3 <<< \"open('x', 'r+').write('y')\"",
            "cat <<'EOF' | python3\nimport os\nopen(os.path.join('src', 'a.rs'), 'w').write('')\nEOF",
            "node -e \"require('fs').writeFileSync('package.json', '{}')\"",
            "node <<'EOF'\nconst fs = require('fs');\nfs.appendFileSync('a.js', 'x');\nEOF",
            // A script saved first and run in a later command.
            "cat > $TMPDIR/fix.py <<'EOF'\np = 'crates/x.rs'\nwith open(p, 'w') as f:\n    f.write('')\nEOF",
        ]);
        assert_eq!(
            writes("python3 -c \"open('a', 'w')\"").as_deref(),
            Some("its Python script writes files")
        );
    }

    #[test]
    fn absolute_paths_and_cd_are_followed() {
        assert_blocked(&[
            "echo x > /runs/r1/workspace/src/a.rs",
            "cd src && echo x > a.rs",
            "echo x > ./src/../src/a.rs",
            "echo x > \"$PWD/a.rs\"",
        ]);
        assert_allowed(&[
            "cd /tmp && echo x > a.txt",
            "cd \"$TMPDIR\" && echo x > a.txt",
            "(cd /tmp && echo x > a.txt)",
            "echo x > /runs/r1/workspace-2/a.txt",
            "echo x > ../outside.txt",
        ]);
        // The subshell ends, and with it the `cd`.
        assert_blocked(&["(cd /tmp && echo x > a.txt); echo y > b.txt"]);
    }

    #[test]
    fn ordinary_commands_go_through() {
        assert_allowed(&[
            "cargo test --workspace --no-fail-fast",
            "cargo fmt --all",
            "pnpm test && pnpm typecheck",
            "git status",
            "git add -A && git commit -m 'Fix > and < in names'",
            "npx prettier --write src",
            "make build",
            "python3 -m pytest tests",
            "python3 scripts/generate.py",
            "node scripts/build.js",
            "ls -la src | head -20",
            "grep -rn '>' src | wc -l",
            "echo \"a > b\"",
        ]);
    }

    #[test]
    fn output_sent_to_temporary_files_or_nowhere_goes_through() {
        assert_allowed(&[
            "cargo test > $TMPDIR/test.log 2>&1",
            "cargo test > \"${TMPDIR}/test.log\" 2>&1",
            "cargo build > /dev/null 2>&1",
            "make 2>/dev/null",
            "echo done >&2",
            "cargo test 2>&1 | tail -20",
            "pnpm test 2>&1 | tee \"$TMPDIR/out.txt\"",
            "cargo test &> /dev/null",
            "ls > /tmp/listing.txt",
            "exec 3>&1",
            "cmd >&-",
            "diff <(git show HEAD:a.rs) a.rs",
        ]);
    }

    #[test]
    fn here_documents_for_commands_that_write_nothing_go_through() {
        assert_allowed(&[
            "git commit -F - <<'EOF'\nFix it > now\nEOF",
            "git commit -m \"$(cat <<'EOF'\nDon't break > things\n\nCo-Authored-By: x\nEOF\n)\"",
            "gh pr create --body \"$(cat <<'EOF'\nIt's done. Open the file with open(p, 'w')\nEOF\n)\"",
            "python3 - <<'EOF'\nimport json\nprint(json.load(open('package.json'))['version'])\nEOF",
            "cat <<EOF\nhello > there\nEOF",
            "node -e \"console.log(require('fs').readFileSync('package.json', 'utf8'))\"",
            "python3 -c \"import sys; print(open(sys.argv[1]).read())\" README.md",
        ]);
    }

    #[test]
    fn scripts_that_write_only_temporary_files_go_through() {
        assert_allowed(&[
            "python3 -c \"import tempfile; open(tempfile.mktemp(), 'w').write('x')\"",
            "python3 - <<'EOF'\nimport os\nopen(os.environ['TMPDIR'] + '/o.json', 'w').write('{}')\nEOF",
            "python3 - \"$TMPDIR/out.txt\" <<'EOF'\nimport sys\nopen(sys.argv[1], 'w').write('x')\nEOF",
            "cat > $TMPDIR/check.py <<'EOF'\nprint(open('src/lib.rs').read().count('fn'))\nEOF",
            "cat > $TMPDIR/notes.md <<'EOF'\nopen(p, 'w')\nEOF",
            "cat > $TMPDIR/repro.py <<'EOF'\nimport os\nopen(os.environ['TMPDIR'] + '/db', 'w')\nEOF",
            "cd /tmp && python3 -c \"open('a', 'w')\"",
        ]);
    }

    #[test]
    fn reading_with_sed_or_perl_and_editing_outside_the_project_go_through() {
        assert_allowed(&[
            "sed -n '1,20p' src/lib.rs",
            "sed 's/a/b/' src/lib.rs | head",
            "sed -e 's/i/j/' src/lib.rs",
            "sed -E 's/(a)/\\1b/' src/lib.rs",
            "perl -ne 'print if /x/' src/lib.rs",
            "perl -Ilib -e 'print 1'",
            "sed -i '' 's/a/b/' $TMPDIR/scratch.txt",
            "sed -i '' 's/a/b/' /tmp/scratch.txt",
            "find . -name '*.rs' -exec grep -l foo {} +",
        ]);
    }

    #[test]
    fn comparisons_are_not_redirections() {
        assert_allowed(&[
            "[[ \"$a\" > \"$b\" ]] && echo yes",
            "(( 3 > 2 )) && echo yes",
            "echo $(( 3 > 2 ))",
            "test 2 \\> 1",
            "for ((i = 0; i < 3; i++)); do echo $i; done",
        ]);
    }

    #[test]
    fn what_cannot_be_told_goes_through() {
        assert_allowed(&[
            "echo x > \"$OUT\"",
            "sed -i '' 's/a/b/' \"$f\"",
            "echo 'unterminated > quote",
            "echo x >",
            "echo x > ~/notes.txt",
        ]);
    }

    #[test]
    fn git_files_are_not_project_files() {
        assert_allowed(&[
            "cat > .git/COMMIT_EDITMSG <<'EOF'\nFix it\nEOF",
            "echo x >> .git/info/exclude",
        ]);
        assert_blocked(&["echo x >> .gitignore"]);
    }

    #[test]
    fn the_hook_turns_a_command_back_with_a_reason_and_lets_the_rest_through() {
        let workspace = Path::new(WORKSPACE);
        let event = |command: &str| {
            serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": { "command": command, "description": "x" },
                "cwd": WORKSPACE,
            })
            .to_string()
        };
        let reason = hook(workspace, &event("sed -i '' 's/a/b/' src/lib.rs")).unwrap_err();
        assert!(reason.contains("sed -i changes src/lib.rs"), "{reason}");
        assert!(reason.contains("Edit or Write"), "{reason}");
        assert_eq!(hook(workspace, &event("cargo test")), Ok(()));
        assert_eq!(hook(workspace, "not json"), Ok(()));
        assert_eq!(hook(workspace, r#"{"tool_input":{}}"#), Ok(()));
        // The command runs where Claude Code says it does.
        let elsewhere = serde_json::json!({
            "tool_input": { "command": "echo x > a.txt" },
            "cwd": "/tmp",
        })
        .to_string();
        assert_eq!(hook(workspace, &elsewhere), Ok(()));
    }
}
