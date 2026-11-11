//! How many tests a check skipped, from what its test tools print. A check's exit code says whether
//! it passed, not whether everything in it ran: Rust's own harness, pytest, jest, vitest, `go test`
//! and rspec all end well with tests skipped. Only their own summaries count. Output with none of
//! them says nothing, rather than a number that could be wrong.

use std::borrow::Cow;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;

/// More than any summary needs. The rest of a longer line is passed over, so one huge line costs
/// no memory.
const MAX_LINE: usize = 4096;

/// What a check printed, its output and then its errors. `None` if Keel recognises no test tool's
/// summary in it, or can't read all of it. A file that isn't there is nothing printed.
pub fn in_files(paths: &[&Path]) -> Option<u32> {
    let mut tally = Tally::default();
    for path in paths {
        match std::fs::File::open(path) {
            Ok(file) => read_lines(BufReader::new(file), &mut tally).ok()?,
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    tally.total()
}

#[cfg(test)]
fn in_text(text: &str) -> Option<u32> {
    let mut tally = Tally::default();
    read_lines(text.as_bytes(), &mut tally).unwrap();
    tally.total()
}

/// One line at a time, without holding more than `MAX_LINE` of one.
fn read_lines(mut reader: impl BufRead, tally: &mut Tally) -> std::io::Result<()> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let end = buffer.iter().position(|&byte| byte == b'\n');
        let piece = &buffer[..end.unwrap_or(buffer.len())];
        let room = MAX_LINE.saturating_sub(line.len());
        line.extend_from_slice(&piece[..piece.len().min(room)]);
        let used = end.map_or(buffer.len(), |at| at + 1);
        reader.consume(used);
        if end.is_some() {
            tally.line(&String::from_utf8_lossy(&line));
            line.clear();
        }
    }
    if !line.is_empty() {
        tally.line(&String::from_utf8_lossy(&line));
    }
    Ok(())
}

#[derive(Default)]
struct Tally {
    /// A summary of pytest, jest, vitest or rspec, or of Rust's harness, was there.
    summarised: bool,
    /// What those summaries said was skipped.
    in_summaries: u64,
    /// Rust's harness summarised a run, so what `keel_harness::needs!` wrote is from its tests.
    rust: bool,
    /// Tests `keel_harness::needs!` stopped. Rust's harness counts them as passed.
    stopped: u64,
    /// `go test -v` said each test it ran.
    go_verbose: bool,
    go_skipped: u64,
}

impl Tally {
    fn line(&mut self, line: &str) {
        // A progress line can be written over with a carriage return, and the summary come after.
        for part in without_colours(line).split('\r') {
            self.part(part.trim_end());
        }
    }

    fn part(&mut self, part: &str) {
        let trimmed = part.trim();
        if let Some(rest) = trimmed.strip_prefix("test result: ") {
            if let Some(ignored) = rust_ignored(rest) {
                self.rust = true;
                self.summary(ignored);
            }
        } else if trimmed.starts_with("=== RUN ") {
            self.go_verbose = true;
        } else if trimmed.starts_with("--- SKIP: ") {
            self.go_skipped += 1;
        } else if stopped_by_needs(part) {
            self.stopped += 1;
        } else if let Some(skipped) = pytest(trimmed)
            .or_else(|| jest(trimmed))
            .or_else(|| vitest(trimmed))
            .or_else(|| rspec(trimmed))
        {
            self.summary(skipped);
        }
    }

    fn summary(&mut self, skipped: u64) {
        self.summarised = true;
        self.in_summaries += skipped;
    }

    fn total(&self) -> Option<u32> {
        let mut total = self.in_summaries;
        if self.rust {
            total += self.stopped;
        }
        if self.go_verbose {
            total += self.go_skipped;
        }
        (self.summarised || self.go_verbose).then(|| u32::try_from(total).unwrap_or(u32::MAX))
    }
}

/// The line without the codes that colour it in a terminal.
fn without_colours(line: &str) -> Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut plain = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            plain.push(c);
        } else if chars.clone().next() == Some('[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(plain)
}

/// "3 skipped", as a number and a word.
fn number_and_word(item: &str) -> Option<(u64, &str)> {
    let (number, word) = item.trim().split_once(' ')?;
    Some((number.parse().ok()?, word))
}

/// What each of `items` counts, if every one of them is a number and one of `words`.
fn counts<'a>(items: impl Iterator<Item = &'a str>, words: &[&str]) -> Option<Vec<(u64, &'a str)>> {
    let counts: Option<Vec<_>> = items
        .map(|item| number_and_word(item).filter(|(_, word)| words.contains(word)))
        .collect();
    counts.filter(|counts| !counts.is_empty())
}

fn sum_of(counts: &[(u64, &str)], word: &str) -> u64 {
    counts
        .iter()
        .filter(|(_, said)| *said == word)
        .map(|(count, _)| count)
        .sum()
}

/// Rust's harness: `ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in
/// 0.00s`. An ignored test didn't run, so it counts as skipped. What was filtered out, someone
/// chose to leave out.
fn rust_ignored(rest: &str) -> Option<u64> {
    let (_, counts) = rest.split_once(". ")?;
    let mut items = counts.split("; ");
    let passed = number_and_word(items.next()?)?;
    let failed = number_and_word(items.next()?)?;
    let ignored = number_and_word(items.next()?)?;
    (passed.1 == "passed" && failed.1 == "failed" && ignored.1 == "ignored").then_some(ignored.0)
}

/// `skipped <test>: <why>`, as `keel_harness::testing::skipped` writes it. When the output and the
/// errors share a stream, it can start part way through a line of the harness: after `test name
/// ... `, or after the dots of `-q`.
fn stopped_by_needs(line: &str) -> bool {
    line.match_indices("skipped ").any(|(at, word)| {
        let before = &line[..at];
        let after_the_harness =
            before.ends_with(" ... ") || before.chars().all(|c| matches!(c, '.' | 'i' | 'F'));
        let Some((test, why)) = line[at + word.len()..].split_once(": ") else {
            return false;
        };
        after_the_harness
            && !test.is_empty()
            && test
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
            && !why.trim().is_empty()
    })
}

/// pytest: `1 passed, 3 skipped, 1 xfailed in 0.01s`, between rows of `=` unless it ran with
/// `-q`. Deselected tests were left out on purpose, with `-k` or `-m`.
fn pytest(line: &str) -> Option<u64> {
    let line = line.trim_matches(|c| c == '=' || c == ' ');
    let (items, time) = line.rsplit_once(" in ")?;
    let seconds = time.split(' ').next()?.strip_suffix('s')?;
    seconds.parse::<f64>().ok()?;
    let words = [
        "passed",
        "failed",
        "skipped",
        "deselected",
        "xfailed",
        "xpassed",
        "warning",
        "warnings",
        "error",
        "errors",
        "rerun",
    ];
    Some(sum_of(&counts(items.split(", "), &words)?, "skipped"))
}

/// jest: `Tests:       3 skipped, 1 todo, 2 passed, 6 total`. A test still to write is not one
/// that was skipped.
fn jest(line: &str) -> Option<u64> {
    let items = line.strip_prefix("Tests:")?.trim();
    let counts = counts(
        items.split(", "),
        &["failed", "skipped", "todo", "passed", "total"],
    )?;
    (counts.last()?.1 == "total").then(|| sum_of(&counts, "skipped"))
}

/// vitest: `Tests  2 passed | 3 skipped | 1 todo (6)`.
fn vitest(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("Tests ")?.trim();
    let (items, total) = rest.strip_suffix(')')?.rsplit_once(" (")?;
    total.parse::<u64>().ok()?;
    let words = ["failed", "passed", "expected fail", "skipped", "todo"];
    Some(sum_of(&counts(items.split(" | "), &words)?, "skipped"))
}

/// rspec: `4 examples, 0 failures, 3 pending`. Pending is both what it skipped and what it ran
/// expecting it to fail, and neither proves anything.
fn rspec(line: &str) -> Option<u64> {
    let mut items = line.split(", ");
    let (_, examples) = number_and_word(items.next()?)?;
    let (_, failures) = number_and_word(items.next()?)?;
    if !matches!(examples, "example" | "examples") || !matches!(failures, "failure" | "failures") {
        return None;
    }
    let mut pending = 0;
    for item in items {
        match number_and_word(item)? {
            (count, "pending") => pending += count,
            (_, rest) if rest.ends_with("occurred outside of examples") => {}
            _ => return None,
        }
    }
    Some(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each sample is what the tool really printed, cut down to the lines that matter and with the
    // folder names shortened. Only `go test` is written from its format, since Go isn't installed
    // where these were taken.

    /// `cargo test` on a crate with a test that `keel_harness::needs!` stopped, inside a sandbox
    /// that refuses `/bin/ps`. Its output and its errors, as Keel keeps them, in two files.
    const CARGO_STDOUT: &str = "
running 56 tests
test process::tests::a_pid_is_recognised_only_with_its_start_time ... ok
test process::tests::exit_codes_come_through ... ok

test result: ok. 56 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 2.69s

";
    const CARGO_STDERR: &str = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.07s
     Running unittests src/lib.rs (target/debug/deps/keel_harness-094002dde62cf509)
skipped process::tests::a_pid_is_recognised_only_with_its_start_time: it runs /bin/ps, which is setuid, and the sandbox this runs in refuses that (Operation not permitted (os error 1))
";

    /// A test binary with one test `#[ignore]`d and one stopped by `needs!`, with its output and its
    /// errors in one stream: the line `needs!` writes can start part way through another.
    const LIBTEST_ONE_STREAM: &str = "
running 3 tests
test adds ... ok
test needs_a_sandbox ... skipped needs_a_sandbox: it starts a sandbox, and macOS refuses that inside the one this runs in (sandbox-exec: sandbox_apply: Operation not permitted)
ok
test slow_one ... ignored, slow

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
    const LIBTEST_QUIET: &str = "
running 3 tests
iskipped needs_a_sandbox: it starts a sandbox, and macOS refuses that inside the one this runs in (sandbox-exec: sandbox_apply: Operation not permitted)
..
test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

    /// pytest 8.4.2, with `@pytest.mark.skip`, `skipif`, `pytest.skip()` and an `xfail`.
    const PYTEST: &str =
        "============================= test session starts ==============================
platform darwin -- Python 3.9.6, pytest-8.4.2, pluggy-1.6.0
rootdir: /tmp/py
collected 5 items

test_sample.py .sssx                                                     [100%]

=================== 1 passed, 3 skipped, 1 xfailed in 0.01s ====================
";
    const PYTEST_QUIET: &str =
        ".sssx                                                                    [100%]
=========================== short test summary info ============================
SKIPPED [1] test_sample.py:7: needs a database
SKIPPED [1] test_sample.py:11: not on macOS
SKIPPED [1] test_sample.py:16: no network here
1 passed, 3 skipped, 1 xfailed in 0.01s
";
    /// Leaving tests out with `-k` isn't skipping them: whoever wrote the command chose it.
    const PYTEST_DESELECTED: &str = "collected 5 items / 4 deselected / 1 selected

test_sample.py .                                                         [100%]

======================= 1 passed, 4 deselected in 0.00s ========================
";

    /// jest 30.5.2, with `test.skip`, a `describe.skip` of two and a `test.todo`. It writes this to
    /// its errors, not its output.
    const JEST: &str = "
Test Suites: 2 passed, 2 total
Tests:       3 skipped, 1 todo, 2 passed, 6 total
Snapshots:   0 total
Time:        0.246 s
Ran all test suites.
";

    /// vitest 5.1.0, with the same tests.
    const VITEST: &str = "
 RUN  v5.1.0 /tmp/vt


 Test Files  2 passed (2)
      Tests  2 passed | 3 skipped | 1 todo (6)
   Start at  12:28:20
   Duration  78ms (transform 51%, import 29%, worker 12%, tests 8%)

";

    /// `go test -v`, with `t.Skip` in a test and in a subtest.
    const GO_VERBOSE: &str = "=== RUN   TestAdd
--- PASS: TestAdd (0.00s)
=== RUN   TestDatabase
    sample_test.go:12: needs a database
--- SKIP: TestDatabase (0.00s)
=== RUN   TestPlatform
=== RUN   TestPlatform/linux
    sample_test.go:18: not on macOS
=== RUN   TestPlatform/darwin
--- PASS: TestPlatform (0.00s)
    --- SKIP: TestPlatform/linux (0.00s)
    --- PASS: TestPlatform/darwin (0.00s)
PASS
ok  \texample.com/sample\t0.123s
";
    /// Without `-v`, `go test` doesn't say what it skipped.
    const GO_QUIET: &str = "ok  \texample.com/sample\t0.123s\n";

    /// rspec 3.13, with `xit`, `skip` and `pending`.
    const RSPEC: &str = ".***

Pending: (Failures listed here are expected and do not affect your suite's status)

  1) sample needs a database
     # Temporarily skipped with xit
     # ./sample_spec.rb:6

Finished in 0.00312 seconds (files took 0.03605 seconds to load)
4 examples, 0 failures, 3 pending

";

    #[test]
    fn what_needs_stopped_in_rust_counts_as_skipped_from_the_two_files_keel_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let stdout = dir.path().join("check-1.log");
        let stderr = dir.path().join("check-1.stderr.txt");
        std::fs::write(&stdout, CARGO_STDOUT).unwrap();
        std::fs::write(&stderr, CARGO_STDERR).unwrap();
        assert_eq!(in_files(&[&stdout, &stderr]), Some(1));
        // Its errors alone have no summary, so they say nothing.
        assert_eq!(in_files(&[&stderr]), None);
        assert_eq!(in_files(&[&stdout]), Some(0));
    }

    #[test]
    fn rust_counts_what_it_ignored_and_what_needs_stopped_even_part_way_through_a_line() {
        assert_eq!(in_text(LIBTEST_ONE_STREAM), Some(2));
        assert_eq!(in_text(LIBTEST_QUIET), Some(2));
        // Every test binary has its own summary.
        let workspace = format!("{LIBTEST_ONE_STREAM}{CARGO_STDOUT}{CARGO_STDERR}");
        assert_eq!(in_text(&workspace), Some(3));
    }

    #[test]
    fn pytest_counts_what_it_skipped_and_not_what_was_left_out_on_purpose() {
        assert_eq!(in_text(PYTEST), Some(3));
        assert_eq!(in_text(PYTEST_QUIET), Some(3));
        assert_eq!(in_text(PYTEST_DESELECTED), Some(0));
        assert_eq!(
            in_text("==== 3 passed, 1 warning in 62.11s (0:01:02) ===="),
            Some(0)
        );
    }

    #[test]
    fn jest_and_vitest_count_what_they_skipped() {
        assert_eq!(in_text(JEST), Some(3));
        assert_eq!(in_text(VITEST), Some(3));
        assert_eq!(in_text("Tests:       2 passed, 2 total"), Some(0));
        assert_eq!(in_text("      Tests  1 failed | 4 passed (5)"), Some(0));
    }

    #[test]
    fn go_counts_only_when_it_says_each_test() {
        assert_eq!(in_text(GO_VERBOSE), Some(2));
        assert_eq!(in_text(GO_QUIET), None);
    }

    #[test]
    fn rspec_counts_what_is_pending() {
        assert_eq!(in_text(RSPEC), Some(3));
        assert_eq!(in_text("12 examples, 1 failure"), Some(0));
    }

    #[test]
    fn colours_and_carriage_returns_do_not_hide_a_summary() {
        let coloured = "\u{1b}[1mTests:       \u{1b}[22m\u{1b}[1m\u{1b}[33m3 skipped\u{1b}[39m\u{1b}[22m, \u{1b}[1m\u{1b}[32m2 passed\u{1b}[39m\u{1b}[22m, 5 total\r\n";
        assert_eq!(in_text(coloured), Some(3));
        assert_eq!(
            in_text("running...\r 1 passed, 2 skipped in 0.30s"),
            Some(2)
        );
    }

    #[test]
    fn several_tools_in_one_check_add_up() {
        assert_eq!(in_text(&format!("{PYTEST}{VITEST}{RSPEC}")), Some(9));
    }

    #[test]
    fn what_no_tool_summarised_says_nothing_rather_than_none_skipped() {
        for text in [
            "",
            "all good\n",
            "ok\n",
            // Words that look like it, from something else.
            "skipped optional dependency: fsevents\n",
            "Tests: none yet\n",
            "3 passed, 2 skipped\n",
            "--- SKIP: TestX (0.00s)\n",
        ] {
            assert_eq!(in_text(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_long_log_is_read_a_line_at_a_time_and_a_huge_line_does_not_get_in_the_way() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("check-1.log");
        let mut text = String::new();
        text.push_str(&"x".repeat(5_000_000));
        text.push('\n');
        for _ in 0..1000 {
            text.push_str(LIBTEST_ONE_STREAM);
        }
        // Without a line break at the end.
        text.push_str("1 passed, 4 skipped in 0.01s");
        std::fs::write(&log, text).unwrap();
        assert_eq!(in_files(&[&log]), Some(2004));
        assert_eq!(in_files(&[&dir.path().join("missing.log")]), None);
    }
}
