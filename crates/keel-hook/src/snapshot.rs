//! The project's state around a command: what `git status` lists, with a fingerprint of each of
//! those paths, and the commit `HEAD` names. Only paths git lists are looked at, so nothing git
//! ignores and nothing outside the project. A link is known by where it points and never
//! followed, on the way to a path or at its end.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use crate::git::Git;

/// How much a snapshot may cost. Past `entries` there's no snapshot, and past the byte limits a
/// file is known by its size and time instead of its content.
#[derive(Clone, Copy)]
pub struct Limits {
    pub entries: usize,
    pub file_bytes: u64,
    pub total_bytes: u64,
    /// For the whole of one hook call, well inside what Claude Code gives it.
    pub time: Duration,
}

impl Limits {
    pub const DEFAULT: Self = Self {
        entries: 5000,
        file_bytes: 4 << 20,
        total_bytes: 64 << 20,
        time: Duration::from_millis(3500),
    };
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    Missing,
    Dir,
    /// Where a link points, hashed.
    Link(u64),
    /// Anything that isn't a plain file, a folder or a link, never read.
    Special,
    File {
        size: u64,
        mtime: i64,
        /// The content's hash, when it was within the limits.
        hash: Option<u64>,
    },
}

/// What comparing two fingerprints can tell.
#[derive(PartialEq)]
enum Verdict {
    Same,
    Changed,
    /// Written again at the same size, too large to read: it may or may not have changed.
    Rewritten,
}

impl Mark {
    fn compare(&self, other: &Mark) -> Verdict {
        match (self, other) {
            (
                Mark::File { size, mtime, hash },
                Mark::File {
                    size: other_size,
                    mtime: other_mtime,
                    hash: other_hash,
                },
            ) => {
                if size != other_size {
                    return Verdict::Changed;
                }
                match (hash, other_hash) {
                    (Some(hash), Some(other_hash)) if hash == other_hash => Verdict::Same,
                    (Some(_), Some(_)) => Verdict::Changed,
                    _ if mtime == other_mtime => Verdict::Same,
                    _ => Verdict::Rewritten,
                }
            }
            _ if self == other => Verdict::Same,
            _ => Verdict::Changed,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub path: String,
    /// Git's two status letters, `??` for a file it doesn't track.
    pub xy: String,
    pub mark: Mark,
}

impl Entry {
    fn tracked(&self) -> bool {
        self.xy != "??"
    }
}

#[derive(Debug, PartialEq)]
pub struct Snapshot {
    /// The commit `HEAD` named, if there was one.
    pub head: Option<String>,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Modified,
    Added,
    Deleted,
    /// Still there, but git no longer tracks it, as after `git rm --cached`.
    Untracked,
    /// Written again, too large to tell whether its content changed.
    Rewritten,
}

#[derive(Debug, PartialEq)]
pub struct Change {
    pub path: String,
    pub kind: Kind,
    /// Git tracks it, before or after, so `git diff` against the commit can show it.
    pub tracked: bool,
}

/// What changed from one snapshot to the next.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub list: Vec<Change>,
    /// The commit `HEAD` names moved, as with a commit, a checkout or a reset.
    pub head_moved: bool,
    /// Something couldn't be compared, so "nothing changed" can't be said.
    pub unsure: bool,
    /// Files changed state that can't be told one by one: too many, as after a checkout of an
    /// orphan branch, or with no commit to compare them with.
    pub uncounted: bool,
}

impl Changes {
    fn push(&mut self, path: &str, kind: Kind, tracked: bool) {
        self.list.push(Change {
            path: path.to_owned(),
            kind,
            tracked,
        });
    }
}

/// Budget left for reading files in one hook call.
struct Budget {
    bytes: u64,
    deadline: Instant,
}

pub fn take(git: &Git, limits: &Limits, deadline: Instant) -> Option<Snapshot> {
    let out = git.run(
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--no-ahead-behind",
            "--untracked-files=all",
            "--ignore-submodules=all",
            "--no-renames",
        ],
        deadline,
    )?;
    let status = parse_status(&out)?;
    if status.listed.len() > limits.entries {
        return None;
    }
    let root = File::open(git.workspace).ok()?;
    let mut budget = Budget {
        bytes: limits.total_bytes,
        deadline,
    };
    let entries = status
        .listed
        .into_iter()
        .map(|(xy, path)| {
            let mark = mark(&root, &path, limits, &mut budget);
            Entry { path, xy, mark }
        })
        .collect();
    Some(Snapshot {
        head: status.head,
        entries,
    })
}

/// What `status --porcelain=v2 -z --branch` says.
struct Status {
    head: Option<String>,
    /// Each path with its two status letters.
    listed: Vec<(String, String)>,
}

fn parse_status(out: &[u8]) -> Option<Status> {
    let text = std::str::from_utf8(out).ok()?;
    let mut head = None;
    let mut listed = Vec::new();
    let mut records = text.split('\0').filter(|record| !record.is_empty());
    while let Some(record) = records.next() {
        let fields: Vec<&str> = record.splitn(2, ' ').collect();
        match fields.as_slice() {
            ["#", header] => {
                if let Some(oid) = header.strip_prefix("branch.oid ") {
                    head = (oid != "(initial)").then(|| oid.to_owned());
                }
            }
            ["1", rest] => {
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                listed.push((parts.first()?.to_string(), parts.get(7)?.to_string()));
            }
            ["2", rest] => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                listed.push((parts.first()?.to_string(), parts.get(8)?.to_string()));
                // Where it was renamed from comes as its own record.
                listed.push(("D.".to_owned(), records.next()?.to_owned()));
            }
            ["u", rest] => {
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                listed.push((parts.first()?.to_string(), parts.get(9)?.to_string()));
            }
            ["?", path] => listed.push(("??".to_owned(), (*path).to_owned())),
            _ => {}
        }
    }
    Some(Status { head, listed })
}

fn mark(root: &File, path: &str, limits: &Limits, budget: &mut Budget) -> Mark {
    let mut file = match at::open(root, path) {
        at::Opened::File(file) => file,
        at::Opened::Link(target) => return Mark::Link(fnv(fnv_start(), &target)),
        at::Opened::Missing => return Mark::Missing,
        at::Opened::Other => return Mark::Special,
    };
    let Ok(meta) = file.metadata() else {
        return Mark::Missing;
    };
    if meta.is_dir() {
        return Mark::Dir;
    }
    if !meta.is_file() {
        return Mark::Special;
    }
    let size = meta.len();
    let mtime = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
    let hash = if size <= limits.file_bytes && size <= budget.bytes {
        budget.bytes -= size;
        hash_file(&mut file, budget.deadline)
    } else {
        None
    };
    Mark::File { size, mtime, hash }
}

/// Reads what's already open, so nothing can swap the file underneath.
fn hash_file(file: &mut File, deadline: Instant) -> Option<u64> {
    let mut hash = fnv_start();
    let mut buffer = vec![0u8; 64 << 10];
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        match file.read(&mut buffer).ok()? {
            0 => return Some(hash),
            read => hash = fnv(hash, &buffer[..read]),
        }
    }
}

/// Opening a path one folder at a time, from an open folder, refusing a link at every step: a
/// link put in place of a folder can't lead the hook out of the project.
mod at {
    use std::ffi::{CString, c_char, c_int};
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    unsafe extern "C" {
        fn openat(fd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
        fn readlinkat(fd: c_int, path: *const c_char, buf: *mut c_char, size: usize) -> isize;
    }

    #[cfg(target_os = "macos")]
    mod sys {
        pub const NOFOLLOW: i32 = 0x0100;
        pub const NONBLOCK: i32 = 0x0004;
        pub const DIRECTORY: i32 = 0x0010_0000;
        pub const CLOEXEC: i32 = 0x0100_0000;
        pub const ELOOP: i32 = 62;
    }
    #[cfg(not(target_os = "macos"))]
    mod sys {
        pub const NOFOLLOW: i32 = 0o400_000;
        pub const NONBLOCK: i32 = 0o4_000;
        pub const DIRECTORY: i32 = 0o200_000;
        pub const CLOEXEC: i32 = 0o2_000_000;
        pub const ELOOP: i32 = 40;
    }
    const ENOENT: i32 = 2;
    const ENOTDIR: i32 = 20;

    pub enum Opened {
        File(File),
        /// Where the link points.
        Link(Vec<u8>),
        Missing,
        /// There, but not something to open, like a socket.
        Other,
    }

    fn open_at(dir: c_int, name: &CString, flags: c_int) -> Result<OwnedFd, i32> {
        let fd = unsafe { openat(dir, name.as_ptr(), flags | sys::CLOEXEC) };
        if fd < 0 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    pub fn open(root: &File, relative: &str) -> Opened {
        let names: Option<Vec<CString>> = relative
            .split('/')
            .map(|name| {
                (!name.is_empty() && name != "." && name != "..")
                    .then(|| CString::new(name).ok())
                    .flatten()
            })
            .collect();
        let Some(names) = names else {
            return Opened::Missing;
        };
        let Some((last, folders)) = names.split_last() else {
            return Opened::Missing;
        };
        let mut folder: Option<OwnedFd> = None;
        for name in folders {
            let parent = folder.as_ref().map_or(root.as_raw_fd(), AsRawFd::as_raw_fd);
            match open_at(parent, name, sys::DIRECTORY | sys::NOFOLLOW) {
                Ok(next) => folder = Some(next),
                Err(_) => return Opened::Missing,
            }
        }
        let parent = folder.as_ref().map_or(root.as_raw_fd(), AsRawFd::as_raw_fd);
        // Never waits on a pipe either.
        match open_at(parent, last, sys::NOFOLLOW | sys::NONBLOCK) {
            Ok(fd) => Opened::File(File::from(fd)),
            Err(errno) if errno == sys::ELOOP => {
                let mut target = vec![0u8; 4096];
                let read = unsafe {
                    readlinkat(
                        parent,
                        last.as_ptr(),
                        target.as_mut_ptr().cast(),
                        target.len(),
                    )
                };
                match usize::try_from(read) {
                    Ok(read) => {
                        target.truncate(read);
                        Opened::Link(target)
                    }
                    Err(_) => Opened::Missing,
                }
            }
            Err(ENOENT | ENOTDIR) => Opened::Missing,
            Err(_) => Opened::Other,
        }
    }
}

fn fnv_start() -> u64 {
    0xcbf2_9ce4_8422_2325
}

/// FNV-1a: only ever compared with itself, to tell whether a file changed.
fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Each path once, the way git listed it first: a tracked entry comes before an untracked one.
fn by_path(snapshot: &Snapshot) -> HashMap<&str, &Entry> {
    let mut paths = HashMap::new();
    for entry in &snapshot.entries {
        paths.entry(entry.path.as_str()).or_insert(entry);
    }
    paths
}

/// Paths `git rm --cached` left behind: gone from the index and untracked, so listed twice.
fn no_longer_tracked(snapshot: &Snapshot) -> HashSet<&str> {
    let deleted: HashSet<&str> = snapshot
        .entries
        .iter()
        .filter(|entry| entry.xy.starts_with('D'))
        .map(|entry| entry.path.as_str())
        .collect();
    snapshot
        .entries
        .iter()
        .filter(|entry| !entry.tracked() && deleted.contains(entry.path.as_str()))
        .map(|entry| entry.path.as_str())
        .collect()
}

/// What changed from `before` to `after`.
pub fn compare(
    before: &Snapshot,
    after: &Snapshot,
    git: &Git,
    limits: &Limits,
    deadline: Instant,
) -> Changes {
    let mut changes = Changes {
        head_moved: before.head != after.head,
        ..Changes::default()
    };
    let listed_before = by_path(before);
    let listed_after = by_path(after);
    let untracked_before = no_longer_tracked(before);
    let untracked_after = no_longer_tracked(after);
    for path in &untracked_after {
        if !untracked_before.contains(path) {
            changes.push(path, Kind::Untracked, true);
        }
    }
    let mut newly_listed = Vec::new();
    let mut seen = HashSet::new();
    for entry in &after.entries {
        let path = entry.path.as_str();
        if !seen.insert(path) || untracked_after.contains(path) {
            continue;
        }
        match listed_before.get(path) {
            Some(old) => sort(
                &mut changes,
                entry,
                &old.mark,
                entry.tracked() || old.tracked(),
            ),
            None => newly_listed.push(entry),
        }
    }
    // No longer listed: committed, put back, or deleted and committed. Only the content says.
    let root = File::open(git.workspace).ok();
    let mut budget = Budget {
        bytes: limits.total_bytes,
        deadline,
    };
    for (path, old) in &listed_before {
        if listed_after.contains_key(path) {
            continue;
        }
        let now = Entry {
            path: (*path).to_owned(),
            xy: old.xy.clone(),
            mark: match &root {
                Some(root) => mark(root, path, limits, &mut budget),
                None => Mark::Missing,
            },
        };
        sort(&mut changes, &now, &old.mark, old.tracked());
    }
    // Newly listed and git didn't track it: it's new.
    let (untracked, tracked): (Vec<&Entry>, Vec<&Entry>) =
        newly_listed.into_iter().partition(|entry| !entry.tracked());
    for entry in untracked {
        changes.push(&entry.path, Kind::Added, false);
    }
    // Newly listed and tracked: it was as the commit had it, so the commit tells what it was.
    // Past the cap, or with no commit to ask, nothing is said file by file.
    if !tracked.is_empty() {
        let paths: Vec<&str> = tracked.iter().map(|entry| entry.path.as_str()).collect();
        let differ = before
            .head
            .as_deref()
            .filter(|_| paths.len() <= MOST_NAMED)
            .and_then(|head| differ_from(git, head, &paths, deadline));
        match differ {
            Some(differ) => {
                for (letter, path) in differ {
                    changes.push(&path, kind_of(letter), true);
                }
            }
            None => changes.uncounted = true,
        }
    }
    // The commit moved, so files git lists neither time can have changed too: an edit committed
    // in the same command, or a checkout. The two commits tell which.
    if changes.head_moved {
        let between = match (&before.head, &after.head) {
            (Some(old), Some(new)) => between_commits(git, old, new, deadline),
            _ => None,
        };
        match between {
            Some(between) => {
                // Paths git listed either time were told above; the cap is for the rest.
                let unlisted: Vec<(char, String)> = between
                    .into_iter()
                    .filter(|(_, path)| {
                        !listed_before.contains_key(path.as_str())
                            && !listed_after.contains_key(path.as_str())
                    })
                    .collect();
                if unlisted.len() > MOST_NAMED {
                    changes.uncounted = true;
                } else {
                    for (letter, path) in unlisted {
                        changes.push(&path, kind_of(letter), true);
                    }
                }
            }
            None => changes.unsure = true,
        }
    }
    changes.list.sort_by(|a, b| a.path.cmp(&b.path));
    changes.list.dedup_by(|a, b| a.path == b.path);
    changes
}

/// Past this many files from one question to git, none is named.
const MOST_NAMED: usize = 500;

fn kind_of(letter: char) -> Kind {
    match letter {
        'A' => Kind::Added,
        'D' => Kind::Deleted,
        _ => Kind::Modified,
    }
}

/// A path listed before, compared with how it is now.
fn sort(changes: &mut Changes, now: &Entry, before: &Mark, tracked: bool) {
    let kind = match before.compare(&now.mark) {
        Verdict::Same => return,
        Verdict::Rewritten => Kind::Rewritten,
        Verdict::Changed if now.mark == Mark::Missing => Kind::Deleted,
        Verdict::Changed if *before == Mark::Missing => Kind::Added,
        Verdict::Changed => Kind::Modified,
    };
    changes.push(&now.path, kind, tracked);
}

/// Of these paths, the ones whose content differs from what `head` had, with git's letter for
/// how: `A`, `D` or `M`.
fn differ_from(
    git: &Git,
    head: &str,
    paths: &[&str],
    deadline: Instant,
) -> Option<Vec<(char, String)>> {
    let mut args = vec![
        "diff",
        "--name-status",
        "-z",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--ignore-submodules=all",
        head,
        "--",
    ];
    args.extend_from_slice(paths);
    name_status(&git.run(&args, deadline)?)
}

/// What differs between two commits, with git's letter for how.
fn between_commits(
    git: &Git,
    old: &str,
    new: &str,
    deadline: Instant,
) -> Option<Vec<(char, String)>> {
    let out = git.run(
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            old,
            new,
        ],
        deadline,
    )?;
    name_status(&out)
}

fn name_status(out: &[u8]) -> Option<Vec<(char, String)>> {
    let text = std::str::from_utf8(out).ok()?;
    let mut records = text.split('\0').filter(|record| !record.is_empty());
    let mut differ = Vec::new();
    while let (Some(status), Some(path)) = (records.next(), records.next()) {
        differ.push((status.chars().next()?, path.to_owned()));
    }
    Some(differ)
}

/// A snapshot as a file: NUL-separated, so any path survives.
pub fn encode(snapshot: &Snapshot) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |text: &str| {
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    };
    field("keel-snapshot-1");
    field(snapshot.head.as_deref().unwrap_or("-"));
    for entry in &snapshot.entries {
        field(&entry.path);
        field(&entry.xy);
        field(&match &entry.mark {
            Mark::Missing => "missing".to_owned(),
            Mark::Dir => "dir".to_owned(),
            Mark::Special => "special".to_owned(),
            Mark::Link(hash) => format!("link {hash}"),
            Mark::File { size, mtime, hash } => match hash {
                Some(hash) => format!("file {size} {mtime} {hash}"),
                None => format!("file {size} {mtime} -"),
            },
        });
    }
    out
}

pub fn decode(bytes: &[u8]) -> Option<Snapshot> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut fields = text.split('\0');
    (fields.next()? == "keel-snapshot-1").then_some(())?;
    let head = Some(fields.next()?.to_owned()).filter(|head| head != "-");
    let mut entries = Vec::new();
    while let Some(path) = fields.next() {
        if path.is_empty() {
            break;
        }
        let xy = fields.next()?.to_owned();
        let words: Vec<&str> = fields.next()?.split(' ').collect();
        let mark = match words.as_slice() {
            ["missing"] => Mark::Missing,
            ["dir"] => Mark::Dir,
            ["special"] => Mark::Special,
            ["link", hash] => Mark::Link(hash.parse().ok()?),
            ["file", size, mtime, hash] => Mark::File {
                size: size.parse().ok()?,
                mtime: mtime.parse().ok()?,
                hash: if *hash == "-" {
                    None
                } else {
                    Some(hash.parse().ok()?)
                },
            },
            _ => return None,
        };
        entries.push(Entry {
            path: path.to_owned(),
            xy,
            mark,
        });
    }
    Some(Snapshot { head, entries })
}
