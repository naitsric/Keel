//! Reads the user's Claude Code session and version. Keel never reads, stores or forwards a
//! credential: `claude auth status` reports account facts and no token.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::command::clean_env;
use crate::process::stdout_within;

/// How long `claude` gets to say what it is and who's signed in. It normally takes a second or two,
/// and nothing that depends on the answer should wait on it for good.
pub const CHECK_LIMIT: Duration = Duration::from_secs(20);

/// The oldest version M0 was run against.
pub const MIN_VERSION: (u32, u32, u32) = (2, 1, 318);
/// The first version Keel refuses without testing. Claude Code ships new patch versions
/// constantly, so the check is at the minor level; each step also verifies what `system/init`
/// reports, which is what catches a behaviour change inside this range.
pub const BELOW_VERSION: (u32, u32, u32) = (2, 2, 0);

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub logged_in: bool,
    pub method: Option<String>,
    pub provider: Option<String>,
    pub subscription: Option<String>,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub expires_at: Option<String>,
}

impl AuthStatus {
    pub fn read(claude: &Path, path_env: &str) -> Option<Self> {
        match ask(claude, path_env, &["auth", "status", "--json"], CHECK_LIMIT) {
            Answer::Said(text) => Self::parse(&text),
            Answer::Failed | Answer::TimedOut => None,
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        let text_of = |key: &str| value.get(key).and_then(|v| v.as_str()).map(str::to_owned);
        Some(Self {
            logged_in: value
                .get("loggedIn")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            method: text_of("authMethod"),
            provider: text_of("apiProvider"),
            subscription: text_of("subscriptionType"),
            email: text_of("email"),
            org_name: text_of("orgName"),
            expires_at: value
                .get("loginExpiresAt")
                .filter(|v| !v.is_null())
                .map(|v| v.to_string().trim_matches('"').to_owned()),
        })
    }

    /// How an attempt records who it ran as. Identifies the account, holds no secret.
    pub fn account_label(&self) -> Option<String> {
        let email = self.email.as_deref()?;
        Some(match &self.subscription {
            Some(plan) => format!("{email} ({plan})"),
            None => email.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum VersionCheck {
    Supported { found: String },
    TooOld { found: String, minimum: String },
    Untested { found: String, below: String },
    Unknown,
}

pub fn read_version(claude: &Path, path_env: &str) -> Option<String> {
    match ask(claude, path_env, &["--version"], CHECK_LIMIT) {
        Answer::Said(text) => Some(text.trim().to_owned()),
        Answer::Failed | Answer::TimedOut => None,
    }
}

enum Answer {
    Said(String),
    /// It couldn't even start.
    Failed,
    /// It started and didn't finish in time.
    TimedOut,
}

fn ask(claude: &Path, path_env: &str, args: &[&str], limit: Duration) -> Answer {
    let mut command = Command::new(claude);
    command.args(args).env_clear().envs(clean_env(path_env));
    match stdout_within(command, limit) {
        Ok(Some(bytes)) => Answer::Said(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => Answer::TimedOut,
        Err(_) => Answer::Failed,
    }
}

/// What `claude` says about itself, in one look.
#[derive(Debug, Clone, Default)]
pub struct SessionCheck {
    pub version: Option<String>,
    pub auth: Option<AuthStatus>,
    /// It didn't answer in time, so `version` and `auth` say nothing about the session.
    pub not_responding: bool,
}

/// `limit` applies to each question. If the first gets no answer the second isn't asked, since it
/// would only wait the same way.
pub fn check_session(claude: &Path, path_env: &str, limit: Duration) -> SessionCheck {
    let version = match ask(claude, path_env, &["--version"], limit) {
        Answer::Said(text) => Some(text.trim().to_owned()),
        Answer::Failed => None,
        Answer::TimedOut => {
            return SessionCheck {
                not_responding: true,
                ..SessionCheck::default()
            };
        }
    };
    match ask(claude, path_env, &["auth", "status", "--json"], limit) {
        Answer::Said(text) => SessionCheck {
            version,
            auth: AuthStatus::parse(&text),
            not_responding: false,
        },
        Answer::Failed => SessionCheck {
            version,
            ..SessionCheck::default()
        },
        Answer::TimedOut => SessionCheck {
            version,
            auth: None,
            not_responding: true,
        },
    }
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text.split_whitespace().next()?;
    let mut parts = word.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

fn dotted(v: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

pub fn check_version(text: &str) -> VersionCheck {
    let Some(version) = parse_version(text) else {
        return VersionCheck::Unknown;
    };
    let found = dotted(version);
    if version < MIN_VERSION {
        VersionCheck::TooOld {
            found,
            minimum: dotted(MIN_VERSION),
        }
    } else if version >= BELOW_VERSION {
        VersionCheck::Untested {
            found,
            below: dotted(BELOW_VERSION),
        }
    } else {
        VersionCheck::Supported { found }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_range() {
        assert!(matches!(
            check_version("2.1.318 (Claude Code)"),
            VersionCheck::Supported { .. }
        ));
        assert!(matches!(
            check_version("2.1.999 (Claude Code)"),
            VersionCheck::Supported { .. }
        ));
        assert!(matches!(
            check_version("2.1.300 (Claude Code)"),
            VersionCheck::TooOld { .. }
        ));
        assert!(matches!(
            check_version("2.2.0 (Claude Code)"),
            VersionCheck::Untested { .. }
        ));
        assert_eq!(check_version("garbage"), VersionCheck::Unknown);
    }

    /// A `claude` that runs `script`, already run once so the tests time only the script. It
    /// answers `--warm-up` before the script, since some scripts here never answer at all.
    fn fake_claude(dir: &Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("claude");
        let text = format!("#!/bin/bash\n[ \"$1\" = --warm-up ] && exit 0\n{script}\n");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::testing::warm_up(&path, &["--warm-up"]);
        path
    }

    const QUICK: Duration = Duration::from_millis(300);

    #[test]
    fn a_claude_that_answers_is_read_as_usual() {
        let dir = tempfile::tempdir().unwrap();
        let claude = fake_claude(
            dir.path(),
            r#"[ "$1" = "--version" ] && echo '2.1.318 (Claude Code)' || echo '{"loggedIn":true}'"#,
        );
        let check = check_session(&claude, "/usr/bin:/bin", Duration::from_secs(5));
        assert_eq!(check.version.as_deref(), Some("2.1.318 (Claude Code)"));
        assert!(check.auth.unwrap().logged_in);
        assert!(!check.not_responding);
    }

    #[test]
    fn a_claude_that_hangs_is_stopped_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let claude = fake_claude(dir.path(), "exec sleep 60");
        let started = std::time::Instant::now();
        let check = check_session(&claude, "/usr/bin:/bin", QUICK);
        assert!(check.not_responding);
        assert!(check.version.is_none());
        assert!(started.elapsed() < Duration::from_secs(5), "it asked once");
    }

    #[test]
    fn a_session_check_that_hangs_keeps_the_version_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let claude = fake_claude(
            dir.path(),
            r#"[ "$1" = "--version" ] && { echo '2.1.318 (Claude Code)'; exit 0; }; exec sleep 60"#,
        );
        // Plenty for the first answer, now that the script has run once, even on a busy machine.
        let check = check_session(&claude, "/usr/bin:/bin", Duration::from_secs(2));
        assert_eq!(check.version.as_deref(), Some("2.1.318 (Claude Code)"));
        assert!(check.auth.is_none());
        assert!(check.not_responding);
    }

    #[test]
    fn a_claude_that_cannot_start_is_not_the_same_as_one_that_hangs() {
        let check = check_session(Path::new("/nonexistent/claude"), "/usr/bin:/bin", QUICK);
        assert!(!check.not_responding);
        assert!(check.version.is_none());
        assert!(check.auth.is_none());
    }

    #[test]
    fn auth_status_keeps_only_account_facts() {
        let status = AuthStatus::parse(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max",
                "email":"a@example.com","orgName":"Acme","loginExpiresAt":null,"accessToken":"never-kept"}"#,
        )
        .unwrap();
        assert!(status.logged_in);
        assert_eq!(
            status.account_label().as_deref(),
            Some("a@example.com (max)")
        );
        assert!(status.expires_at.is_none());
        assert!(!format!("{status:?}").contains("never-kept"));
    }

    #[test]
    fn logged_out() {
        let status = AuthStatus::parse(r#"{"loggedIn":false}"#).unwrap();
        assert!(!status.logged_in);
        assert!(status.account_label().is_none());
    }
}
