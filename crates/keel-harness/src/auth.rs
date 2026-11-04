//! Reads the user's Claude Code session and version. Keel never reads, stores or forwards a
//! credential: `claude auth status` reports account facts and no token.

use std::path::Path;
use std::process::Command;

use serde::Serialize;

use crate::command::clean_env;

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
        let output = Command::new(claude)
            .args(["auth", "status", "--json"])
            .env_clear()
            .envs(clean_env(path_env))
            .output()
            .ok()?;
        Self::parse(&String::from_utf8_lossy(&output.stdout))
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
    let output = Command::new(claude)
        .arg("--version")
        .env_clear()
        .envs(clean_env(path_env))
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
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
