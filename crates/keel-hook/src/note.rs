//! What the agent reads next to a command's result. Changed files are named as a fact, and an
//! edit that changed nothing is the one #51 was about: a replacement from Bash that matched
//! nothing and said nothing.

use crate::snapshot::{Changes, Kind};

/// How many paths a list names before it says how many more there are.
const NAMED: usize = 5;

const HINT: &str = "Edit and Write fail out loud when a replacement matches nothing; an edit \
                    from Bash says nothing.";

/// `base` is the commit the changes are told against: `HEAD`, or the one it named before the
/// command when the command moved it.
pub fn note(changes: &Changes, edit_like: bool, base: &str) -> Option<String> {
    let hint = |text: String| {
        if edit_like {
            format!("{text} {HINT}")
        } else {
            text
        }
    };
    if changes.uncounted {
        // A moved commit leaves a clean tree, so only the commits can show what changed.
        let see = if changes.head_moved {
            format!("`git diff --stat {base}` and `git status --short` show them")
        } else {
            "`git status --short` shows them".to_owned()
        };
        return Some(hint(format!(
            "While this Bash command ran, files in the project changed state; {see}."
        )));
    }
    if changes.list.is_empty() {
        // A moved commit, or one that couldn't be compared, may have changed files unseen.
        return (edit_like && !changes.head_moved && !changes.unsure).then(|| {
            hint(
                "This Bash command changed none of the files git watches here; ignored files, \
                 submodules and nested repositories aren't watched."
                    .to_owned(),
            )
        });
    }
    let of = |kind: Kind| -> Vec<&str> {
        changes
            .list
            .iter()
            .filter(|change| change.kind == kind)
            .map(|change| change.path.as_str())
            .collect()
    };
    let mut kinds = Vec::new();
    for (name, kind) in [
        ("modified", Kind::Modified),
        ("new", Kind::Added),
        ("deleted", Kind::Deleted),
        ("no longer tracked", Kind::Untracked),
        ("rewritten, too large to compare", Kind::Rewritten),
    ] {
        let paths = of(kind);
        if !paths.is_empty() {
            kinds.push(format!("{name}: {}", list(&paths)));
        }
    }
    // Against the commit and with the path from the top, `git diff` shows a change from any
    // folder, staged or not, and one committed by the command itself.
    let tracked: Vec<&str> = changes
        .list
        .iter()
        .filter(|change| change.tracked && change.kind != Kind::Untracked)
        .map(|change| change.path.as_str())
        .collect();
    let mut checks = Vec::new();
    if !tracked.is_empty() {
        let paths: Vec<String> = tracked
            .iter()
            .take(NAMED)
            .map(|path| quote(&format!(":(top,literal){path}")))
            .collect();
        checks.push(format!("`git diff {base} -- {}`", paths.join(" ")));
    }
    if tracked.len() < changes.list.len() {
        checks.push("`git status --short`".to_owned());
    }
    // A file written again at its size, too large to read, may not have changed at all.
    let definite = changes
        .list
        .iter()
        .any(|change| change.kind != Kind::Rewritten);
    let what = if definite {
        "files in the project changed"
    } else {
        "files in the project were written"
    };
    Some(hint(format!(
        "While this Bash command ran, {what} ({}). Check them with {}.",
        kinds.join("; "),
        checks.join(" and "),
    )))
}

fn list(paths: &[&str]) -> String {
    let mut shown: Vec<String> = paths.iter().take(NAMED).map(|path| quote(path)).collect();
    if paths.len() > NAMED {
        shown.push(format!("and {} more", paths.len() - NAMED));
    }
    shown.join(", ")
}

/// A path as the shell reads it back, whatever is in it.
pub fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// One command of a line, crudely: its words with quotes taken out, where its output goes, and
/// the text it reads on stdin from a here-document or a here-string.
#[derive(Default)]
struct Simple {
    words: Vec<String>,
    targets: Vec<String>,
    stdin: Vec<String>,
    /// Its stdin is the output of the command before it.
    piped: bool,
}

/// Whether a command looks like it meant to change files. Crude on purpose: it only decides
/// whether "changed none of the files git watches" is worth saying, together with the hint about
/// Edit, so it leans toward ordinary commands saying nothing.
///
/// It counts an in-place editor (`sed`, `perl`, `ruby -i`, `gawk -i inplace`), output sent into
/// a file that isn't a device or temporary, a script run from a temporary folder (how the builder
/// in run 40c0fe47 ran its fixes), and inline code whose text writes or replaces something.
pub fn looks_like_edit(command: &str) -> bool {
    looks_like_edit_at(command, 0)
}

fn looks_like_edit_at(command: &str, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    let commands = simple_commands(command);
    commands.iter().enumerate().any(|(at, simple)| {
        let fed = at
            .checked_sub(1)
            .and_then(|before| commands.get(before))
            .filter(|_| simple.piped);
        simple.targets.iter().any(|target| !elsewhere(target)) || edits(simple, fed, depth)
    })
}

/// The command cut into simple commands at `|`, `;`, `&`, parentheses and new lines, outside
/// quotes, reading here-documents as it goes.
fn simple_commands(text: &str) -> Vec<Simple> {
    // An edit shows near the start; a command of megabytes shouldn't cost seconds to look at.
    let chars: Vec<char> = text.chars().take(64 << 10).collect();
    let mut reader = Reader {
        chars,
        at: 0,
        commands: vec![Simple::default()],
        word: String::new(),
        in_word: false,
        test: false,
        heredocs: Vec::new(),
    };
    reader.run();
    reader.commands
}

struct Reader {
    chars: Vec<char>,
    at: usize,
    commands: Vec<Simple>,
    word: String,
    in_word: bool,
    /// Inside `[[ … ]]`, where `<` and `>` compare.
    test: bool,
    /// Here-documents whose bodies start on the next line: the command they belong to, the
    /// delimiter, and whether leading tabs go.
    heredocs: Vec<(usize, String, bool)>,
}

impl Reader {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn current(&mut self) -> &mut Simple {
        if self.commands.is_empty() {
            self.commands.push(Simple::default());
        }
        let last = self.commands.len() - 1;
        &mut self.commands[last]
    }

    fn end_word(&mut self) {
        if !self.in_word {
            return;
        }
        self.in_word = false;
        let word = std::mem::take(&mut self.word);
        match word.as_str() {
            "[[" => self.test = true,
            "]]" => self.test = false,
            _ => {}
        }
        self.current().words.push(word);
    }

    fn end_command(&mut self, piped: bool) {
        self.end_word();
        let current = self.current();
        if !current.words.is_empty() || !current.targets.is_empty() || !current.stdin.is_empty() {
            self.commands.push(Simple::default());
        }
        self.test = false;
        self.current().piped = piped;
    }

    /// The word starting here, with quotes taken out.
    fn read_word(&mut self) -> String {
        let mut word = String::new();
        let mut quote: Option<char> = None;
        while let Some(c) = self.peek(0) {
            match quote {
                Some(open) if c == open => quote = None,
                Some(_) => word.push(c),
                None if c == '\'' || c == '"' => quote = Some(c),
                None if c.is_whitespace()
                    || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>') =>
                {
                    break;
                }
                None => word.push(c),
            }
            self.at += 1;
        }
        word
    }

    /// From `((` to its `))`, kept as one word: `<` and `>` there compare numbers.
    fn arithmetic(&mut self) {
        let mut level = 0usize;
        while let Some(c) = self.peek(0) {
            self.at += 1;
            self.word.push(c);
            match c {
                '(' => level += 1,
                ')' => {
                    level = level.saturating_sub(1);
                    if level == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        self.in_word = true;
    }

    /// The bodies of the here-documents opened on the line just ended.
    fn read_heredocs(&mut self) {
        for (command, delimiter, strip) in std::mem::take(&mut self.heredocs) {
            let mut body = String::new();
            while self.at < self.chars.len() {
                let start = self.at;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.at += 1;
                }
                let line: String = self.chars[start..self.at].iter().collect();
                if self.peek(0) == Some('\n') {
                    self.at += 1;
                }
                let line = if strip {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == delimiter {
                    break;
                }
                body.push_str(line);
                body.push('\n');
            }
            if let Some(simple) = self.commands.get_mut(command) {
                simple.stdin.push(body);
            }
        }
    }

    fn run(&mut self) {
        let mut quote: Option<char> = None;
        while let Some(c) = self.peek(0) {
            if self
                .commands
                .iter()
                .map(|simple| simple.words.len())
                .sum::<usize>()
                > 2000
            {
                break;
            }
            self.at += 1;
            if let Some(open) = quote {
                match c {
                    _ if c == open => quote = None,
                    '\\' if open == '"' => match self.peek(0) {
                        // A line continuation inside double quotes joins the lines.
                        Some('\n') => self.at += 1,
                        Some(next) => {
                            self.word.push(next);
                            self.at += 1;
                        }
                        None => {}
                    },
                    _ => self.word.push(c),
                }
                continue;
            }
            match c {
                '\'' | '"' => {
                    quote = Some(c);
                    self.in_word = true;
                }
                '\\' => match self.peek(0) {
                    // A line continuation joins the lines and is no word.
                    Some('\n') => self.at += 1,
                    Some(next) => {
                        self.word.push(next);
                        self.in_word = true;
                        self.at += 1;
                    }
                    None => {}
                },
                '#' if !self.in_word => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.at += 1;
                    }
                }
                ' ' | '\t' => self.end_word(),
                '$' if self.peek(0) == Some('(') && self.peek(1) == Some('(') => {
                    self.word.push('$');
                    self.arithmetic();
                }
                '(' if !self.in_word && self.peek(0) == Some('(') => {
                    self.at -= 1;
                    self.arithmetic();
                }
                '<' | '>' if self.test => {
                    self.word.push(c);
                    self.in_word = true;
                }
                // An arrow in a word, as in `format:%h->%s`, isn't a redirection.
                '>' if self.in_word && self.word.ends_with(['-', '=']) => self.word.push(c),
                '>' => self.redirection(),
                '<' => self.input(),
                '&' if self.peek(0) == Some('>') => self.end_word(),
                '\n' => {
                    self.end_command(false);
                    self.read_heredocs();
                }
                '|' | ';' | '&' | '(' | ')' | '`' => {
                    let piped = c == '|' && self.peek(0) != Some('|');
                    if self.peek(0) == Some(c) {
                        self.at += 1;
                    }
                    self.end_command(piped);
                }
                _ => {
                    self.word.push(c);
                    self.in_word = true;
                }
            }
        }
        self.end_word();
    }

    fn redirection(&mut self) {
        // Digits right before it name a descriptor, as in `2>`.
        if self.in_word && self.word.chars().all(|c| c.is_ascii_digit()) {
            self.word.clear();
            self.in_word = false;
        }
        self.end_word();
        while matches!(self.peek(0), Some('>' | '|' | '!')) {
            self.at += 1;
        }
        // `>&2` copies a descriptor and `>(…)` is a process: neither is a file.
        if matches!(self.peek(0), Some('&' | '(')) {
            self.at += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit() || c == '-') {
                self.at += 1;
            }
            return;
        }
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.at += 1;
        }
        let target = self.read_word();
        if !target.is_empty() {
            self.current().targets.push(target);
        }
    }

    /// After a `<`: a here-string, a here-document, or a file to read.
    fn input(&mut self) {
        self.end_word();
        if self.peek(0) == Some('<') && self.peek(1) == Some('<') {
            self.at += 2;
            while matches!(self.peek(0), Some(' ' | '\t')) {
                self.at += 1;
            }
            let text = self.read_word();
            self.current().stdin.push(text);
        } else if self.peek(0) == Some('<') {
            self.at += 1;
            let strip = self.peek(0) == Some('-');
            if strip {
                self.at += 1;
            }
            while matches!(self.peek(0), Some(' ' | '\t')) {
                self.at += 1;
            }
            let delimiter = self.read_word();
            if !delimiter.is_empty() {
                let command = self.commands.len().saturating_sub(1);
                self.heredocs.push((command, delimiter, strip));
            }
        }
    }
}

/// Programs that only start the next one, and words before a command.
const BEFORE_A_COMMAND: [&str; 18] = [
    "env", "command", "builtin", "exec", "nohup", "sudo", "nice", "time", "timeout", "xargs", "!",
    "{", "}", "if", "then", "else", "do", "while",
];

fn edits(simple: &Simple, fed: Option<&Simple>, depth: usize) -> bool {
    if depth > 3 {
        return false;
    }
    let words = &simple.words;
    let mut at = 0;
    // Past assignments, wrappers, and launchers such as `uv run`.
    while let Some(word) = words.get(at) {
        let name = program(word);
        if BEFORE_A_COMMAND.contains(&name)
            || word.starts_with('-')
            || word.starts_with(|c: char| c.is_ascii_digit())
            || (word.contains('=') && !word.starts_with('='))
        {
            at += 1;
        } else if ["uv", "poetry", "pipenv", "pdm", "rye", "hatch", "pixi"].contains(&name)
            && words.get(at + 1).is_some_and(|next| next == "run")
        {
            at += 2;
        } else {
            break;
        }
    }
    let Some(first) = words.get(at) else {
        return false;
    };
    let args = &words[at + 1..];
    // What it reads on stdin: its own here-document or here-string, or what's piped from one.
    let stdin: Vec<&String> = simple
        .stdin
        .iter()
        .chain(fed.map(|fed| fed.stdin.iter()).into_iter().flatten())
        .collect();
    let writing_stdin = stdin.iter().any(|text| writes(text));
    match program(first) {
        "sed" | "gsed" => args.iter().any(|flag| {
            in_place(flag, char::is_ascii_alphabetic) || flag.starts_with("--in-place")
        }),
        "perl" => args
            .iter()
            .any(|flag| in_place(flag, char::is_ascii_lowercase)),
        "ruby" => {
            args.iter()
                .any(|flag| in_place(flag, char::is_ascii_lowercase))
                || runs_writing_code(args, &["-e"], &[], writing_stdin)
        }
        "awk" | "gawk" => {
            args.windows(2)
                .any(|pair| pair[0] == "-i" && pair[1] == "inplace")
                || args
                    .iter()
                    .any(|flag| flag == "-iinplace" || flag.starts_with("--include=inplace"))
        }
        "node" | "nodejs" => runs_writing_code(
            args,
            &["-e", "-p", "--eval", "--print", "-pe"],
            &[
                "-r",
                "--require",
                "--import",
                "--loader",
                "-C",
                "--conditions",
            ],
            writing_stdin,
        ),
        "tee" => args
            .iter()
            .find(|flag| !flag.starts_with('-'))
            .is_some_and(|file| !elsewhere(file)),
        // `find … -exec sed -i … {} +` runs the command after `-exec`.
        "find" => args
            .iter()
            .position(|word| matches!(word.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
            .is_some_and(|at| {
                let rest = &args[at + 1..];
                let end = rest
                    .iter()
                    .position(|word| word == ";" || word == "+")
                    .unwrap_or(rest.len());
                let inner = Simple {
                    words: rest[..end].to_vec(),
                    ..Simple::default()
                };
                edits(&inner, None, depth + 1)
            }),
        // `bash -c "sed -i …"`: the string is a command of its own.
        "sh" | "bash" | "zsh" | "dash" => {
            let script = match args.iter().position(|word| {
                word.starts_with('-') && !word.starts_with("--") && word.contains('c')
            }) {
                Some(at) => args.get(at + 1).map(String::as_str),
                None => stdin.first().map(|text| text.as_str()),
            };
            script.is_some_and(|script| looks_like_edit_at(script, depth + 1))
        }
        name if is_python(name) => python_writes(args, writing_stdin),
        _ => false,
    }
}

/// `-i`, possibly among other single-letter options as in `-pi` or `-0pi`.
fn in_place(flag: &str, letters: fn(&char) -> bool) -> bool {
    flag.starts_with('-')
        && !flag.starts_with("--")
        && flag[1..]
            .chars()
            .skip_while(char::is_ascii_digit)
            .take_while(letters)
            .any(|c| c == 'i')
}

fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_python(name: &str) -> bool {
    ["python", "pypy"].iter().any(|base| {
        name.strip_prefix(base)
            .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == '.'))
    })
}

/// Code that writes or replaces something, by its text alone.
fn writes(code: &str) -> bool {
    const TOKENS: [&str; 8] = [
        "write",
        "replace(",
        "rename",
        "writeFile",
        "appendFile",
        "fileinput",
        "shutil",
        "unlink",
    ];
    const MODES: [&str; 14] = [
        "'w'", "\"w\"", "'wb'", "\"wb\"", "'w+'", "\"w+\"", "'a'", "\"a\"", "'r+'", "\"r+\"",
        "'x'", "\"x\"", "mode=", "'at'",
    ];
    TOKENS.iter().any(|token| code.contains(token))
        || (code.contains("open(") && MODES.iter().any(|mode| code.contains(mode)))
}

/// Python running code that writes, or a script from a temporary folder. Not a module (`-m`,
/// test runners among them) and not one of the project's own scripts.
fn python_writes(args: &[String], writing_stdin: bool) -> bool {
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        at += 1;
        if arg == "-" {
            return writing_stdin;
        }
        if arg == "-W" || arg == "-X" {
            at += 1;
            continue;
        }
        if let Some(flags) = arg.strip_prefix('-') {
            // A module, test runners among them.
            if flags.starts_with('m') {
                return false;
            }
            if flags.chars().all(|c| c.is_ascii_alphabetic()) && flags.ends_with('c') {
                return args.get(at).is_some_and(|code| writes(code));
            }
            // Long options, other flags, and `-W`/`-X` with their value attached.
            continue;
        }
        return elsewhere(arg);
    }
    writing_stdin
}

/// An interpreter running code that writes, or a script from a temporary folder, unless it's
/// running tests.
fn runs_writing_code(
    args: &[String],
    inline: &[&str],
    with_value: &[&str],
    writing_stdin: bool,
) -> bool {
    let mut at = 0;
    while let Some(arg) = args.get(at) {
        at += 1;
        if arg.starts_with("--test") || arg == "test" {
            return false;
        }
        if inline.contains(&arg.as_str()) {
            return args.get(at).is_some_and(|code| writes(code));
        }
        if let Some(code) = arg
            .strip_prefix("--eval=")
            .or_else(|| arg.strip_prefix("--print="))
        {
            return writes(code);
        }
        if with_value.contains(&arg.as_str()) {
            at += 1;
            continue;
        }
        if arg == "-" {
            return writing_stdin;
        }
        if !arg.starts_with('-') {
            return elsewhere(arg);
        }
    }
    writing_stdin
}

/// A device, or a temporary file: not the project.
fn elsewhere(target: &str) -> bool {
    [
        "/dev/",
        "$TMPDIR",
        "${TMPDIR",
        "/tmp",
        "/private/tmp",
        "/var/folders",
    ]
    .iter()
    .any(|start| target.starts_with(start))
}
