//! Finds where in the home folder the programs a project's commands use are installed, so the
//! sandbox can let them be read. The sandbox hides the home folder, and that's where version
//! managers install: nvm, asdf, pyenv, rustup… Keel asks the shell where each program is, with
//! `command -v` on the PATH the person's login shell gave, follows the links to the real file, and
//! proposes that folder, or the manager's whole folder when it's one Keel knows. A person confirms
//! what's proposed. Nothing in the repo runs: the shell starts outside it and reads no settings, and
//! all it's asked is where things are.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::command::clean_env;
use crate::process::stdout_within;
use crate::settings::TOOL_DIRS;

/// A folder in the home folder for a project's commands to read, and the programs that need it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// As a person writes it, from the home folder: `~/.nvm`.
    pub folder: String,
    pub programs: Vec<String>,
}

/// Where credentials are known to be, from the home folder. No folder proposed holds one, or is
/// inside one.
pub const CREDENTIALS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".config/gh",
    ".config/hub",
    ".config/op",
    ".config/rclone",
    ".config/configstore",
    ".config/composer/auth.json",
    ".config/git/credentials",
    ".docker",
    ".kube",
    ".gnupg",
    ".password-store",
    ".terraform.d",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".netrc",
    ".pypirc",
    ".pgpass",
    ".my.cnf",
    ".boto",
    ".s3cfg",
    ".vault-token",
    ".gitconfig",
    ".git-credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".gem/credentials",
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".gradle/gradle.properties",
    ".nuget/NuGet/NuGet.Config",
    ".composer/auth.json",
    ".claude",
    ".claude.json",
    "Library/Keychains",
    "Library/Cookies",
];

/// Folders with too much in them to open up whole, even with no credential known to be there.
const TOO_BROAD: &[&str] = &[
    "",
    ".config",
    ".cache",
    ".local",
    ".local/share",
    ".local/state",
    "Library",
    "Library/Application Support",
    "Library/Caches",
    "Library/Containers",
    "Library/Group Containers",
    "Library/Preferences",
    "Desktop",
    "Documents",
    "Downloads",
];

/// Version managers and installers that keep what they install under one folder, which their shims
/// and links lead back into: the whole folder has to be readable, not only where a program is. Some
/// need another one too.
const MANAGERS: &[(&str, &[&str])] = &[
    (".asdf", &[]),
    (".nvm", &[]),
    (".fnm", &[]),
    (".local/share/fnm", &[]),
    ("Library/Application Support/fnm", &[]),
    (".volta", &[]),
    (".nodenv", &[]),
    (".pyenv", &[]),
    (".rbenv", &[]),
    (".rvm", &[]),
    (".goenv", &[]),
    (".jenv", &[]),
    (".sdkman", &[]),
    (".local/share/mise", &[]),
    (".dotnet", &[]),
    (".bun", &[]),
    (".deno", &[]),
    (".rustup", &[]),
    // rustup's `cargo` and `rustc` are links to rustup, which runs the toolchain it keeps in
    // `~/.rustup`. Never all of `~/.cargo`: its credentials are there.
    (".cargo/bin", &[".rustup"]),
    // pnpm's own installer: a script in `bin` that runs pnpm from the folder above.
    ("Library/pnpm", &[]),
    (".local/share/pnpm", &[]),
];

/// What a program can't run without, besides itself.
const NEEDS: &[(&str, &[&str])] = &[
    ("npm", &["node"]),
    ("npx", &["node"]),
    ("pnpm", &["node"]),
    ("pnpx", &["node"]),
    ("yarn", &["node"]),
    ("corepack", &["node"]),
    ("uv", &["python3"]),
    ("uvx", &["python3"]),
    ("poetry", &["python3"]),
    ("pdm", &["python3"]),
    ("hatch", &["python3"]),
    ("pipenv", &["python3"]),
    ("pip", &["python3"]),
    ("pip3", &["python3"]),
    ("pytest", &["python3"]),
    ("tox", &["python3"]),
    ("nox", &["python3"]),
    ("python", &["python3"]),
    ("cargo", &["rustc"]),
    ("bundle", &["ruby"]),
    ("rake", &["ruby"]),
    ("rspec", &["ruby"]),
    ("rails", &["ruby"]),
    ("gem", &["ruby"]),
    ("composer", &["php"]),
    ("phpunit", &["php"]),
    ("mvn", &["java"]),
    ("mvnw", &["java"]),
    ("gradle", &["java"]),
    ("gradlew", &["java"]),
    ("sbt", &["java"]),
    ("mix", &["elixir", "erl"]),
    ("elixir", &["erl"]),
];

/// Words in front of a command that start the next one: `env FOO=1 make`, `time pytest`, `! grep`.
const WRAPPERS: &[&str] = &[
    "env", "time", "nice", "nohup", "exec", "command", "builtin", "!", "{", "if", "then", "else",
    "elif", "while", "until", "do",
];

/// However many a repo's commands name, a person reads the list.
const MOST_PROGRAMS: usize = 32;
const LONGEST_NAME: usize = 64;

/// How long the shell gets to say where everything is. It takes milliseconds.
const LOOKUP_LIMIT: Duration = Duration::from_secs(5);

/// For each name it's given, its name and where it is. Only full paths: a builtin or a function
/// says only its name, and a relative folder on the PATH is wherever the shell is.
const LOOKUP: &str =
    r#"for p do r=$(command -v "$p") && case "$r" in /*) printf '%s\t%s\n' "$p" "$r";; esac; done"#;

/// A program's name and nothing the shell would read as more: no path, variable or pattern.
fn is_name(word: &str) -> bool {
    let mut chars = word.chars();
    word.len() <= LONGEST_NAME
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The word that names what a command starts, past variables set in front and wrappers.
fn command_word(part: &str) -> Option<&str> {
    let mut wrapped = false;
    for word in part.split_whitespace() {
        let word = word.trim_matches(['"', '\'']);
        if word.is_empty() || is_assignment(word) || (wrapped && word.starts_with('-')) {
            continue;
        }
        if WRAPPERS.contains(&word) {
            wrapped = true;
            continue;
        }
        return Some(word);
    }
    None
}

/// The programs these commands start: the first word of each one chained, piped or in a
/// subshell, and what each needs to run (`pnpm` needs `node`). A path is the repo's own or says
/// where it is, so only what it needs is looked for.
pub fn programs(commands: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        if is_name(name) && found.len() < MOST_PROGRAMS && !found.iter().any(|f| f == name) {
            found.push(name.to_owned());
        }
    };
    for command in commands {
        for part in command.split(['\n', ';', '&', '|', '(', ')', '`']) {
            let Some(word) = command_word(part) else {
                continue;
            };
            let name = word.rsplit('/').next().unwrap_or(word);
            if !word.contains('/') {
                add(word);
            }
            for (program, needs) in NEEDS {
                if *program == name {
                    needs.iter().for_each(|need| add(need));
                }
            }
        }
    }
    found
}

/// Where the shell finds each program on this PATH, as `command -v` says. It's `/bin/sh`, which
/// reads no settings, in `/`, so nothing in a repo is read on the way: not a version manager's file
/// and not a hook. Builtins and what isn't there are left out.
pub fn locate(programs: &[String], path_env: &str) -> Vec<(String, PathBuf)> {
    if programs.is_empty() {
        return Vec::new();
    }
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", LOOKUP, "sh"])
        .args(programs)
        .current_dir("/")
        .env_clear()
        .envs(clean_env(path_env));
    let Ok(Some(bytes)) = stdout_within(command, LOOKUP_LIMIT) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (name, path) = line.split_once('\t')?;
            (programs.iter().any(|p| p == name) && path.starts_with('/'))
                .then(|| (name.to_owned(), PathBuf::from(path)))
        })
        .collect()
}

/// `path` with its folder's links followed, but not its own: where the sandbox sees a link.
fn in_real_folder(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    Some(path.parent()?.canonicalize().ok()?.join(name))
}

/// Every path the sandbox has to let be read to run the program at `path`: where it was found,
/// each link on the way, and the real file.
fn way_to(path: &Path) -> Vec<PathBuf> {
    let mut places = vec![path.to_path_buf()];
    let mut current = path.to_path_buf();
    // As many links as macOS follows before it gives up.
    for _ in 0..32 {
        places.extend(in_real_folder(&current));
        let Ok(target) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(folder) => folder.join(target),
            None => target,
        };
    }
    places.extend(path.canonicalize());
    places
}

/// `path` from the home folder, given as it is or with its links followed. Never with `..`, which
/// could hide where it goes.
fn from_home(path: &Path, homes: &[&Path]) -> Option<PathBuf> {
    let inside = homes.iter().find_map(|home| path.strip_prefix(home).ok())?;
    inside
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| inside.to_path_buf())
}

/// Folders are compared as macOS does, without minding case: `~/.SSH` is `~/.ssh`.
fn lower(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// Whether a folder can be opened up: not one with too much in it, and none with a credential in
/// it or around it.
fn is_safe(folder: &Path) -> bool {
    let folder = lower(folder);
    !TOO_BROAD
        .iter()
        .any(|broad| folder == lower(Path::new(broad)))
        && !CREDENTIALS.iter().any(|credential| {
            let credential = lower(Path::new(credential));
            credential.starts_with(&folder) || folder.starts_with(&credential)
        })
}

/// Whether every project can read this already.
fn always_readable(inside: &Path) -> bool {
    TOOL_DIRS
        .iter()
        .filter_map(|dir| dir.strip_prefix("~/"))
        .any(|dir| inside.starts_with(dir))
}

/// The folder a program at `inside` the home folder needs: its manager's, or its own. A folder on
/// the way that's a link has to be readable itself, so that's the one.
fn folders_for(inside: &Path, home: &Path) -> Vec<PathBuf> {
    if always_readable(inside) {
        return Vec::new();
    }
    let found: Vec<PathBuf> = match MANAGERS.iter().find(|(root, _)| inside.starts_with(root)) {
        Some((root, also)) => std::iter::once(PathBuf::from(root))
            .chain(
                also.iter()
                    .filter(|other| home.join(other).is_dir())
                    .map(PathBuf::from),
            )
            .collect(),
        None => {
            let Some(folder) = inside.parent() else {
                return Vec::new();
            };
            let link = folder
                .ancestors()
                .filter(|above| !above.as_os_str().is_empty())
                .filter(|above| home.join(above).is_symlink())
                .last();
            vec![link.unwrap_or(folder).to_path_buf()]
        }
    };
    found.into_iter().filter(|folder| is_safe(folder)).collect()
}

/// The folders in `home` to let be read for programs found at these paths, each once, with the
/// programs that need it. A program outside the home folder needs nothing: only it is hidden.
pub fn folders(located: &[(String, PathBuf)], home: &Path) -> Vec<Found> {
    let real_home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let homes = [home, real_home.as_path()];
    let mut found: Vec<Found> = Vec::new();
    for (program, path) in located {
        for place in way_to(path) {
            let Some(inside) = from_home(&place, &homes) else {
                continue;
            };
            for folder in folders_for(&inside, &real_home) {
                let folder = format!("~/{}", folder.display());
                match found.iter_mut().find(|f| f.folder == folder) {
                    Some(f) if !f.programs.contains(program) => f.programs.push(program.clone()),
                    Some(_) => {}
                    None => found.push(Found {
                        folder,
                        programs: vec![program.clone()],
                    }),
                }
            }
        }
    }
    found
}

/// What to propose for these commands, found on this PATH, in this home folder.
pub fn find(commands: &[&str], path_env: &str, home: &Path) -> Vec<Found> {
    folders(&locate(&programs(commands), path_env), home)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::write_script;

    #[test]
    fn programs_are_what_each_command_starts_and_what_that_needs() {
        assert_eq!(
            programs(&[
                "pnpm install --frozen-lockfile",
                "pnpm test && cargo test --workspace"
            ]),
            ["pnpm", "node", "cargo", "rustc"]
        );
        // Variables set in front, wrappers, pipes, subshells, and `;`, `||` and new lines.
        assert_eq!(
            programs(&[
                "FOO=1 env -i BAR=2 uv run pytest | tee out.txt; (cd sub && time make test) || \
                 echo $(deno --version)\nbundle exec rspec"
            ]),
            [
                "uv", "python3", "tee", "cd", "make", "echo", "deno", "bundle", "ruby"
            ]
        );
        // A path is the repo's own, or says where it is: only what it needs is looked for.
        assert_eq!(
            programs(&[
                "./gradlew test && .venv/bin/python -m pytest",
                "bin/rails test && vendor/bin/phpunit && /usr/bin/make"
            ]),
            ["java", "python3", "ruby", "php"]
        );
    }

    #[test]
    fn only_plain_names_are_asked_about() {
        // `rm` is a name like any other, and all the shell is asked is where it is.
        let found = programs(&[
            "$PYTHON -m pytest && \"$(rm -rf ~)\" && 'node' x && py*.test && ~/bin/x && \
             -rf && a${b}c && ${CARGO:-cargo} test",
        ]);
        assert_eq!(found, ["rm", "node"]);
        // However many a repo's commands name, a person reads the list.
        let many: Vec<String> = (0..100).map(|n| format!("tool{n}")).collect();
        assert!(programs(&[&many.join(" && ")]).len() <= 32);
    }

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_script(path, "#!/bin/sh\necho fake\n").unwrap();
    }

    #[test]
    fn the_shell_says_where_each_program_is_on_the_path_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        executable(&bin.join("node"));
        let path_env = format!("{}:/usr/bin:/bin", bin.display());
        let names: Vec<String> = ["node", "make", "cd", "nope-not-a-program"]
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(
            locate(&names, &path_env),
            [
                ("node".to_owned(), bin.join("node")),
                ("make".to_owned(), PathBuf::from("/usr/bin/make")),
            ],
            "a builtin like `cd`, and what isn't there, are left out"
        );
        // A folder on the PATH that isn't a full path is wherever the shell happens to be, such as
        // a repo, so nothing found through one counts.
        let relative = format!("{}/bin:/usr/bin", dir.path().file_name().unwrap().display());
        let from_there = Command::new("/bin/sh")
            .args(["-c", "command -v node"])
            .current_dir(dir.path().parent().unwrap())
            .env("PATH", &relative)
            .output()
            .unwrap();
        assert!(
            from_there.status.success(),
            "the shell would find it from there"
        );
        assert_eq!(locate(&["node".to_owned()], &relative), []);
        assert_eq!(locate(&[], &path_env), []);
    }

    /// A home folder with what version managers leave in it, made up: links, shims, and programs
    /// that are scripts.
    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }

        fn program(&self, at: &str) -> PathBuf {
            let path = self.dir.path().join(at);
            executable(&path);
            path
        }

        fn file(&self, at: &str) {
            let path = self.dir.path().join(at);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "secret").unwrap();
        }

        fn link(&self, at: &str, to: &str) -> PathBuf {
            let path = self.dir.path().join(at);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(to, &path).unwrap();
            path
        }

        fn proposed(&self, program: &str, path: PathBuf) -> Vec<String> {
            folders(&[(program.to_owned(), path)], &self.path())
                .into_iter()
                .map(|found| found.folder)
                .collect()
        }
    }

    #[test]
    fn a_known_manager_gets_its_whole_folder_since_its_shims_lead_back_into_it() {
        let home = Home::new();
        let nvm = home.program(".nvm/versions/node/v20.11.0/bin/node");
        assert_eq!(home.proposed("node", nvm), ["~/.nvm"]);
        // asdf's shims are scripts that run asdf, which finds the version it installed.
        let asdf = home.program(".asdf/shims/node");
        assert_eq!(home.proposed("node", asdf), ["~/.asdf"]);
        let pyenv = home.program(".pyenv/shims/python3");
        assert_eq!(home.proposed("python3", pyenv), ["~/.pyenv"]);
        let rbenv = home.program(".rbenv/shims/ruby");
        assert_eq!(home.proposed("ruby", rbenv), ["~/.rbenv"]);
        let volta = home.program(".volta/bin/node");
        assert_eq!(home.proposed("node", volta), ["~/.volta"]);
        home.program(".sdkman/candidates/java/21.0.1-tem/bin/java");
        home.link(".sdkman/candidates/java/current", "21.0.1-tem");
        let java = home.path().join(".sdkman/candidates/java/current/bin/java");
        assert_eq!(home.proposed("java", java), ["~/.sdkman"]);
        let dotnet = home.program(".dotnet/dotnet");
        assert_eq!(home.proposed("dotnet", dotnet), ["~/.dotnet"]);
        let bun = home.program(".bun/bin/bun");
        assert_eq!(home.proposed("bun", bun), ["~/.bun"]);
        let deno = home.program(".deno/bin/deno");
        assert_eq!(home.proposed("deno", deno), ["~/.deno"]);
        // pnpm's own installer: a script in `bin` that runs pnpm from beside it.
        let pnpm = home.program("Library/pnpm/bin/pnpm");
        assert_eq!(home.proposed("pnpm", pnpm), ["~/Library/pnpm"]);
        // fnm's default version is a link to a folder of its own.
        home.program(".fnm/node-versions/v20.11.0/installation/bin/node");
        home.link(
            ".fnm/aliases/default",
            "../node-versions/v20.11.0/installation",
        );
        let fnm = home.path().join(".fnm/aliases/default/bin/node");
        assert_eq!(home.proposed("node", fnm), ["~/.fnm"]);
    }

    #[test]
    fn cargo_gets_its_programs_and_rustups_toolchains_and_not_its_credentials() {
        let home = Home::new();
        home.program(".cargo/bin/rustup");
        let cargo = home.link(".cargo/bin/cargo", "rustup");
        home.file(".cargo/credentials.toml");
        std::fs::create_dir_all(home.path().join(".rustup/toolchains")).unwrap();
        assert_eq!(home.proposed("cargo", cargo), ["~/.cargo/bin", "~/.rustup"]);
    }

    #[test]
    fn mise_and_what_else_keel_always_lets_be_read_need_nothing_more() {
        let home = Home::new();
        home.program(".local/bin/mise");
        let shim = home.link(".local/share/mise/shims/node", "../../../bin/mise");
        assert_eq!(home.proposed("node", shim), Vec::<String>::new());
        let claude = home.program(".claude/local/claude");
        assert_eq!(home.proposed("claude", claude), Vec::<String>::new());
    }

    #[test]
    fn anything_else_gets_the_folder_it_is_in_and_each_one_a_link_goes_through() {
        let home = Home::new();
        let tool = home.program("tools/x/bin/x");
        assert_eq!(home.proposed("x", tool), ["~/tools/x/bin"]);
        // A link in the home folder to a program elsewhere in it: the sandbox has to read both.
        home.program("opt/node-20/bin/node");
        let node = home.link("bin/node", "../opt/node-20/bin/node");
        assert_eq!(home.proposed("node", node), ["~/bin", "~/opt/node-20/bin"]);
        // And to one outside it, which can be read anyway.
        let make = home.link("bin/make", "/usr/bin/make");
        assert_eq!(home.proposed("make", make), ["~/bin"]);
        assert_eq!(
            home.proposed("make", PathBuf::from("/usr/bin/make")),
            Vec::<String>::new()
        );
        // A folder that's a link itself: the link has to be readable too.
        home.program(".local/state/nix/profiles/profile/bin/hello");
        home.link(".nix-profile", ".local/state/nix/profiles/profile");
        let hello = home.path().join(".nix-profile/bin/hello");
        assert_eq!(home.proposed("hello", hello), ["~/.nix-profile"]);
    }

    #[test]
    fn each_folder_comes_once_with_every_program_that_needs_it() {
        let home = Home::new();
        let node = home.program(".nvm/versions/node/v20.11.0/bin/node");
        let npm = home.program(".nvm/versions/node/v20.11.0/bin/npm");
        let tool = home.program("bin/tool");
        assert_eq!(
            folders(
                &[
                    ("node".into(), node),
                    ("tool".into(), tool),
                    ("npm".into(), npm)
                ],
                &home.path()
            ),
            [
                Found {
                    folder: "~/.nvm".into(),
                    programs: vec!["node".into(), "npm".into()],
                },
                Found {
                    folder: "~/bin".into(),
                    programs: vec!["tool".into()],
                },
            ]
        );
    }

    /// The credentials a home folder can have, each with something in it, and programs put where a
    /// proposal could take them in.
    #[test]
    fn no_folder_proposed_holds_a_credential() {
        let home = Home::new();
        for credential in CREDENTIALS {
            home.file(&format!("{credential}/secret"));
        }
        // Some are files, not folders.
        for file in [
            ".npmrc",
            ".netrc",
            ".claude.json",
            ".git-credentials",
            ".pypirc",
        ] {
            let path = home.path().join(file);
            std::fs::remove_dir_all(&path).unwrap();
            std::fs::write(&path, "secret").unwrap();
        }
        let mut tempting: Vec<(String, PathBuf)> = [
            "tool",
            ".ssh/bin/ssh-thing",
            ".aws/bin/aws-thing",
            ".config/gh/bin/gh-thing",
            ".config/tool",
            ".docker/bin/docker",
            ".gnupg/bin/gpg-thing",
            ".kube/bin/kubectl",
            ".claude/bin/thing",
            ".cargo/bin/cargo",
            ".cargo/tool",
            ".gem/ruby/3.3.0/bin/rake",
            ".gem/tool",
            "Library/tool",
            "Library/Keychains/bin/thing",
            "Library/Application Support/tool",
            "Library/Application Support/thing/bin/thing",
        ]
        .iter()
        .map(|at| ((*at).to_owned(), home.program(at)))
        .collect();
        // Links into a credential's folder, a way round with `..`, and another case for the
        // same folder.
        home.program(".aws/tool");
        tempting.push(("aws".into(), home.link("bin/aws", "../.aws/tool")));
        std::fs::create_dir_all(home.path().join("tools")).unwrap();
        tempting.push((
            "dots".into(),
            home.path().join("tools/../.ssh/bin/ssh-thing"),
        ));
        tempting.push(("case".into(), home.path().join(".SSH/bin/ssh-thing")));
        tempting.push(("home".into(), home.path().join("tool")));

        let proposed = folders(&tempting, &home.path());
        let folders: Vec<&str> = proposed.iter().map(|f| f.folder.as_str()).collect();
        // What's safe still comes through.
        for expected in ["~/.cargo/bin", "~/.gem/ruby/3.3.0/bin", "~/bin"] {
            assert!(folders.contains(&expected), "{expected} in {folders:?}");
        }
        for folder in &folders {
            let inside = folder.strip_prefix("~/").expect(folder);
            assert!(!inside.is_empty(), "the home folder itself");
            for broad in [".config", "Library", "Library/Application Support"] {
                assert_ne!(inside, broad);
            }
            let real = home.path().join(inside);
            for credential in CREDENTIALS {
                let credential = home.path().join(credential);
                assert!(
                    !credential.starts_with(&real) && !real.starts_with(&credential),
                    "{folder} and {}",
                    credential.display()
                );
            }
            // And looking in it, without following links out of it, finds none of them.
            let mut waiting = vec![real.clone()];
            while let Some(dir) = waiting.pop() {
                for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                    let path = entry.path();
                    let kind = entry.file_type().unwrap();
                    if kind.is_dir() {
                        waiting.push(path);
                    } else if kind.is_file() {
                        let text = std::fs::read_to_string(&path).unwrap_or_default();
                        assert_ne!(text, "secret", "{} in {folder}", path.display());
                    }
                }
            }
        }
    }
}
