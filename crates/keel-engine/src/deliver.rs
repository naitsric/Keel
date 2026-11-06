//! Turns a finished run into a draft pull request. Keel does this itself, outside any sandbox and
//! with the user's token, so it works from the mirror: no agent can write there, and nothing an
//! agent wrote gets to run with that token.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use keel_core::{AttemptState, PrState, Reason, mask_secrets};
use keel_harness::github::{Gh, GhError, Token};
use keel_store::rusqlite_value::Value;
use keel_store::{Attempt, Project, Run, now};
use serde_json::json;

use crate::git::{Git, GitError, RemoteAuth};
use crate::{Error, Inner, Result, locked};

/// GitHub refuses a description over 65,536 characters, and the whole delivery would fail with it.
/// This stays a little under, in case GitHub counts some characters as more than one.
const PR_BODY_ROOM: usize = 65_000;
/// Less room than this for what was asked isn't worth a fragment of it.
const SMALLEST_FRAGMENT: usize = 200;

/// The URL git reaches a repo at. Only ever the configured host and a repo name from the project.
fn repo_url(base: &str, repo: &str) -> String {
    format!("{}/{repo}.git", base.trim_end_matches('/'))
}

fn delivery_failed(error: GhError) -> Error {
    Error::Blocked(Reason::DeliveryFailed {
        kind: error.kind.to_owned(),
        detail: error.detail,
    })
}

fn git_failed(error: GitError) -> Error {
    match error {
        GitError::UnsafeBranch(branch) => Error::Blocked(Reason::UnsafeBranch { branch }),
        other => Error::Blocked(Reason::DeliveryFailed {
            kind: other.kind().to_owned(),
            detail: other.to_string(),
        }),
    }
}

impl Inner {
    /// Refreshes the mirror from the repo's default branch. Returns that branch's name.
    pub(crate) fn update_mirror_from_github(
        &self,
        git: &Git,
        mirror: &Path,
        account: &str,
        repo: &str,
    ) -> Result<String> {
        let gh = self.gh()?;
        let token = gh.token(account).map_err(delivery_failed)?;
        let info = gh.repo_info(&token, repo).map_err(delivery_failed)?;
        let auth = RemoteAuth {
            gh: &gh.bin,
            token: &token,
            path_env: &gh.path_env,
        };
        git.update_mirror_from_remote(
            mirror,
            &repo_url(&self.config.github_git_base, repo),
            &info.default_branch,
            &auth,
        )
        .map_err(git_failed)?;
        Ok(info.default_branch)
    }

    /// A project's first map comes from its folder, because it has no account yet. With one, runs
    /// start from GitHub, and that can be different code. Bringing it into the mirror is what
    /// marks the lines it changed as out of date. In the background, since the first fetch of a
    /// large repo takes a while. If it fails, the warning is missing and the next run says why.
    pub(crate) fn compare_map_with_github(self: &Arc<Self>, project: &Project) -> Result<()> {
        let (Some(account), Some(repo)) = (&project.github_account, &project.github_repo) else {
            return Ok(());
        };
        let has_map = self
            .store
            .brain_notes(&project.id)?
            .iter()
            .any(|note| note.kind == "map" && note.state == "active");
        let git = locked(&self.tools).git.clone();
        let (true, Some(git)) = (has_map, git) else {
            return Ok(());
        };
        let inner = self.clone();
        let (id, account, repo) = (project.id.clone(), account.clone(), repo.clone());
        std::thread::spawn(move || {
            let _creating = locked(&inner.creating_workspace);
            let mirror = inner.mirror(&id);
            let _ = inner.update_mirror_from_github(&Git::new(git), &mirror, &account, &repo);
        });
        Ok(())
    }

    pub(crate) fn deliver(self: &Arc<Self>, attempt: &Attempt) {
        let _ = self
            .store
            .set_state(&attempt.id, AttemptState::Running, None);
        self.changed(&attempt.run_id);
        match self.try_deliver(attempt) {
            Ok(url) => {
                let _ = self
                    .store
                    .patch_attempt(&attempt.id, &[("result_text", Value::Text(url))]);
                self.end_early(&attempt.id, &attempt.run_id, AttemptState::Succeeded, None);
                let _ = self.advance(&attempt.run_id);
            }
            Err(Error::Blocked(reason)) => self.end_early(
                &attempt.id,
                &attempt.run_id,
                AttemptState::Blocked,
                Some(reason),
            ),
            Err(other) => self.end_early(
                &attempt.id,
                &attempt.run_id,
                AttemptState::Failed,
                Some(Reason::PrepareFailed {
                    detail: other.to_string(),
                }),
            ),
        }
    }

    fn try_deliver(&self, attempt: &Attempt) -> Result<String> {
        let store = &self.store;
        let run = store.run(&attempt.run_id)?;
        let project = store.project(&run.project_id)?;
        let repo = project
            .github_repo
            .clone()
            .ok_or(Error::Blocked(Reason::NoGithubRemote))?;
        let account = project
            .github_account
            .clone()
            .ok_or(Error::Blocked(Reason::GithubAccountMissing))?;
        let gh = self.gh()?;
        let git = Git::new(
            locked(&self.tools)
                .git
                .clone()
                .ok_or(Error::Blocked(Reason::GitMissing))?,
        );

        let workspace = PathBuf::from(&attempt.workspace);
        let mirror = self.mirror(&project.id);
        let branch = git.current_branch(&workspace).map_err(git_failed)?;
        if git.commits_ahead(&mirror, &workspace).map_err(git_failed)? == 0 {
            return Err(Error::Blocked(Reason::NothingToDeliver));
        }

        let token = gh.token(&account).map_err(delivery_failed)?;
        let info = gh.repo_info(&token, &repo).map_err(delivery_failed)?;
        if branch == info.default_branch {
            return Err(Error::Blocked(Reason::UnsafeBranch { branch }));
        }

        git.fetch_run_branch(&mirror, &workspace, &branch)
            .map_err(git_failed)?;
        let auth = RemoteAuth {
            gh: &gh.bin,
            token: &token,
            path_env: &gh.path_env,
        };
        git.push_branch(
            &mirror,
            &repo_url(&self.config.github_git_base, &repo),
            &branch,
            &auth,
        )
        .map_err(git_failed)?;

        // A pull request is read by people who never saw the run, and what it says stays on GitHub.
        let body = mask_secrets(&self.pull_request_body(&run, &project)?);
        let body_file = self.run_dir(&run.id).join("pull-request.md");
        std::fs::write(&body_file, &body)?;
        // An issue's title already names the problem. Otherwise the commit says what changed better
        // than the first line of a request does.
        let title = if run.source_issue.is_some() {
            run.title.clone()
        } else {
            git.first_subject(&mirror, &workspace)
                .filter(|subject| !subject.is_empty() && !subject.starts_with("keel:"))
                .unwrap_or_else(|| run.title.clone())
        };
        let title = mask_secrets(&title);
        let (url, opened_now) = self.open_pull_request(
            &gh,
            &token,
            &repo,
            &branch,
            &info.default_branch,
            &title,
            &body_file,
        )?;

        let number = url.rsplit('/').next().and_then(|n| n.parse::<i64>().ok());
        // One just opened is a draft, with no need to ask. Of one that was already there Keel knows
        // nothing, so the next look finds out, and until then nothing claims anything about it.
        let known = |value: Value| if opened_now { value } else { Value::Null };
        store.patch_run(
            &run.id,
            &[
                ("pr_url", Value::Text(url.clone())),
                ("pr_number", number.map_or(Value::Null, Value::Integer)),
                ("branch", Value::Text(branch.clone())),
                (
                    "pr_state",
                    known(Value::Text(PrState::Draft.as_str().to_owned())),
                ),
                ("pr_state_at", known(Value::Text(now()))),
                ("pr_checked_at", known(Value::Text(now()))),
            ],
        )?;
        store.patch_attempt(&attempt.id, &[("account", Value::Text(account.clone()))])?;
        store.append_event(
            &attempt.id,
            "delivered",
            &json!({ "url": url, "repo": repo, "account": account, "head": branch, "base": info.default_branch, "draft": true }),
        )?;
        Ok(url)
    }

    /// The pull request's URL, and whether this call is what opened it.
    #[allow(clippy::too_many_arguments)]
    fn open_pull_request(
        &self,
        gh: &Gh,
        token: &Token,
        repo: &str,
        head: &str,
        base: &str,
        title: &str,
        body_file: &Path,
    ) -> Result<(String, bool)> {
        match gh.create_draft_pr(token, repo, head, base, title, body_file) {
            Ok(url) => Ok((url, true)),
            // A retry after a partial failure finds the PR already there, which is what it wanted.
            Err(error) if error.detail.contains("already exists") => error
                .detail
                .split_whitespace()
                .find(|w| w.starts_with("http"))
                .map(|url| (url.to_owned(), false))
                .ok_or_else(|| delivery_failed(error)),
            Err(error) => Err(delivery_failed(error)),
        }
    }

    /// The PR's description. It goes to GitHub rather than the UI, so it's written here, in the
    /// run's language.
    fn pull_request_body(&self, run: &Run, project: &Project) -> Result<String> {
        let spanish = run.language.starts_with("es");
        let label = |en: &'static str, es: &'static str| if spanish { es } else { en };
        let attempts = self.store.attempts_of_run(&run.id)?;
        let latest = |id: &str| attempts.iter().rev().find(|a| a.step_id == id);
        let steps: Vec<keel_core::WorkflowStep> =
            serde_json::from_value(run.workflow.clone()).unwrap_or_default();

        let mut body = String::new();
        // The agents that could change files know what changed. After rework there's one per round,
        // and the last one alone would tell only the end of the story.
        let succeeded = |a: &&keel_store::Attempt| {
            a.kind == "agent"
                && a.role == "worker"
                && a.state == AttemptState::Succeeded.as_str()
                && a.result_text.is_some()
        };
        let mut builders: Vec<&keel_store::Attempt> = attempts
            .iter()
            .filter(succeeded)
            .filter(|a| a.tools == keel_core::ToolProfile::Build.as_str())
            .collect();
        if builders.is_empty() {
            builders.extend(attempts.iter().rev().find(succeeded));
        }
        for attempt in &builders {
            if attempt.round > 0 {
                body.push_str(&format!(
                    "**{} {}**\n\n",
                    label("Round", "Ronda"),
                    attempt.round + 1
                ));
            }
            body.push_str(attempt.result_text.as_deref().unwrap_or_default().trim());
            body.push_str("\n\n");
        }
        if let Some(issue) = &run.source_issue
            && let Some(number) = issue.get("number").and_then(|n| n.as_i64())
        {
            body.push_str(&format!("Refs #{number}\n\n"));
        }

        let top = std::mem::take(&mut body);
        let asked =
            crate::workflows::specifying_attempt(&attempts).and_then(crate::workflows::asked_text);

        let answered = crate::specs::answered_in_run(&attempts);
        if !answered.is_empty() {
            body.push_str(&format!(
                "### {}\n\n{}\n\n",
                label("What the person answered", "Lo que respondió la persona"),
                crate::specs::answer_lines(&answered)
            ));
        }

        let files = crate::workflows::files_changed(&attempts);
        body.push_str(&format!("### {}\n\n", label("What changed", "Qué cambió")));
        for file in &files {
            body.push_str(&format!("- `{file}`\n"));
        }
        body.push_str(&format!(
            "\n### {}\n\n",
            label("How it was checked", "Cómo se comprobó")
        ));
        let mut any_check = false;
        for step in &steps {
            if let keel_core::WorkflowStep::Check { id, run: command } = step {
                any_check = true;
                let passed =
                    latest(id).is_some_and(|a| a.state == AttemptState::Succeeded.as_str());
                let mark = if passed {
                    label("passed", "pasó")
                } else {
                    label("did not pass", "no pasó")
                };
                body.push_str(&format!("- `{command}`: {mark}\n"));
            }
        }
        if !any_check {
            body.push_str(&format!(
                "- {}\n",
                label(
                    "No check step in this workflow.",
                    "Este workflow no tiene paso de verificación."
                )
            ));
        }
        body.push_str(&format!("\n### {}\n\n", label("Steps", "Pasos")));
        for attempt in attempts
            .iter()
            .filter(|a| a.kind == "agent" && a.state != AttemptState::Skipped.as_str())
        {
            let model = attempt
                .model_actual
                .as_deref()
                .unwrap_or(&attempt.model_requested);
            let round = if attempt.round > 0 {
                format!(" ({} {})", label("round", "ronda"), attempt.round + 1)
            } else {
                String::new()
            };
            body.push_str(&format!("- `{}`{round}: {model}\n", attempt.step_id));
        }
        let mut wrote_reviews = false;
        for attempt in attempts
            .iter()
            .filter(|a| a.role == "reviewer" && a.round == run.round)
        {
            if !wrote_reviews {
                body.push_str(&format!("\n### {}\n\n", label("Reviews", "Revisiones")));
                wrote_reviews = true;
            }
            let line = if attempt.state == AttemptState::Skipped.as_str() {
                label(
                    "skipped: nothing it covers changed",
                    "omitida: no cambió nada de lo que cubre",
                )
                .to_owned()
            } else {
                attempt
                    .output
                    .as_ref()
                    .and_then(|o| o.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            };
            body.push_str(&format!("- `{}`: {line}\n", attempt.step_id));
        }
        for attempt in attempts.iter().filter(|a| a.kind == "approval") {
            if let Some(decision) = self
                .store
                .decisions(&project.id)?
                .into_iter()
                .find(|d| d.attempt_id == attempt.id)
            {
                body.push_str(&format!(
                    "\n{}: {}\n",
                    label("Note from the person who approved", "Nota de quien aprobó"),
                    decision.text
                ));
            }
        }
        body.push_str(&format!(
            "\n---\n{}\n",
            label(
                "Draft opened by [Keel](https://github.com/naitsric/Keel) after a person approved it. Review before merging.",
                "Borrador abierto por [Keel](https://github.com/naitsric/Keel) después de que una persona lo aprobara. Revísalo antes de mergear.",
            )
        ));
        // Masked before anything is cut: half a secret is one that masking no longer recognizes.
        Ok(fit_body(
            &mask_secrets(&top),
            asked.as_deref().map(mask_secrets).as_deref(),
            &mask_secrets(&body),
            spanish,
        ))
    }
}

/// Up to `limit` characters of `text`, ending at a word if it can, and saying it was cut.
fn cut_with_note(text: &str, limit: usize, spanish: bool) -> String {
    let note = if spanish {
        "_Cortado aquí para caber en el límite de GitHub. El texto completo está en Keel._"
    } else {
        "_Cut here to fit GitHub's limit. The whole text is in Keel._"
    };
    let room = limit.saturating_sub(note.chars().count() + "\n```\n\n".len());
    let kept: String = text.chars().take(room).collect();
    let mut kept = kept
        .rfind(char::is_whitespace)
        .filter(|space| *space > kept.len() / 2)
        .map_or(kept.as_str(), |space| &kept[..space])
        .to_owned();
    // A code block left open would swallow everything after it.
    if kept.matches("```").count() % 2 == 1 {
        kept.push_str("\n```");
    }
    format!("{kept}\n\n{note}")
}

fn details(summary: &str, text: &str) -> String {
    format!("<details>\n<summary>{summary}</summary>\n\n{text}\n\n</details>\n\n")
}

/// What was asked, collapsed, in as much room as there is.
fn asked_section(text: &str, room: usize, spanish: bool) -> String {
    let say = |en: &'static str, es: &'static str| if spanish { es } else { en };
    let summary = say("What was asked", "Qué se pidió");
    let whole = details(summary, text);
    if whole.chars().count() <= room {
        return whole;
    }
    let text_room = room.saturating_sub(details(summary, "").chars().count());
    if text_room < SMALLEST_FRAGMENT {
        return format!(
            "_{}_\n\n",
            say(
                "What was asked was left out to fit GitHub's limit. It's in Keel.",
                "Lo que se pidió quedó fuera para caber en el límite de GitHub. Está en Keel.",
            )
        );
    }
    details(summary, &cut_with_note(text, text_room, spanish))
}

/// Puts the description together so that GitHub takes it. What was asked gives way first, then what
/// the agents wrote, and whatever gives way says so. All of it has to be masked already.
fn fit_body(top: &str, asked: Option<&str>, rest: &str, spanish: bool) -> String {
    let count = |text: &str| text.chars().count();
    let section = asked.map_or_else(String::new, |text| {
        let room = PR_BODY_ROOM.saturating_sub(count(top) + count(rest));
        asked_section(text, room, spanish)
    });
    let top_room = PR_BODY_ROOM.saturating_sub(count(&section) + count(rest));
    let body = if count(top) <= top_room {
        format!("{top}{section}{rest}")
    } else {
        format!(
            "{}\n\n{section}{rest}",
            cut_with_note(top, top_room.saturating_sub(2), spanish)
        )
    };
    if count(&body) <= PR_BODY_ROOM {
        body
    } else {
        // Only a change to thousands of files gets here, and that's still better than no pull request.
        cut_with_note(&body, PR_BODY_ROOM, spanish)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "Cut here to fit GitHub's limit";

    fn chars(text: &str) -> usize {
        text.chars().count()
    }

    #[test]
    fn what_was_asked_is_collapsed_between_what_the_agents_wrote_and_the_rest() {
        let body = fit_body(
            "Fixed it.\n\n",
            Some("add returns the sum"),
            "### What changed\n",
            false,
        );
        assert_eq!(
            body,
            "Fixed it.\n\n<details>\n<summary>What was asked</summary>\n\nadd returns the sum\n\n</details>\n\n### What changed\n"
        );
        let body = fit_body("", Some("la suma"), "", true);
        assert!(body.contains("<summary>Qué se pidió</summary>"), "{body}");
        assert_eq!(fit_body("top\n", None, "rest\n", false), "top\nrest\n");
    }

    #[test]
    fn what_was_asked_gives_way_first_and_says_so() {
        let body = fit_body(
            "Fixed it.\n\n",
            Some(&"word ".repeat(30_000)),
            "### What changed\n",
            false,
        );
        assert!(chars(&body) <= PR_BODY_ROOM, "{}", chars(&body));
        assert!(body.starts_with("Fixed it."), "what the agents wrote stays");
        assert!(body.ends_with("### What changed\n"), "and so does the rest");
        assert!(body.contains(NOTE), "it says so");
        assert!(body.contains("</details>"), "the section still closes");
    }

    #[test]
    fn spanish_counts_characters_and_not_bytes() {
        let body = fit_body("", Some(&"ñ".repeat(PR_BODY_ROOM - 100)), "", true);
        assert!(
            !body.contains("Cortado"),
            "it fits, though it's twice as many bytes"
        );
        let body = fit_body("", Some(&"ñ".repeat(PR_BODY_ROOM)), "", true);
        assert!(chars(&body) <= PR_BODY_ROOM);
        assert!(
            body.contains("Cortado aquí"),
            "{}",
            &body[body.len() - 200..]
        );
    }

    #[test]
    fn with_no_room_at_all_what_was_asked_leaves_a_line_and_the_agents_text_is_what_gets_cut() {
        let top = "fixed ".repeat(20_000);
        let body = fit_body(
            &top,
            Some("add returns the sum"),
            "### What changed\n",
            false,
        );
        assert!(chars(&body) <= PR_BODY_ROOM, "{}", chars(&body));
        assert!(!body.contains("add returns the sum"));
        assert!(body.contains("What was asked was left out"), "it says so");
        assert!(body.contains(NOTE), "and about the cut too");
        assert!(body.ends_with("### What changed\n"), "the rest stays");
    }

    #[test]
    fn a_change_too_long_to_list_still_makes_a_description_github_takes() {
        let body = fit_body(
            "top\n",
            Some("asked"),
            &"- `a/file.rs`\n".repeat(10_000),
            false,
        );
        assert!(chars(&body) <= PR_BODY_ROOM, "{}", chars(&body));
        assert!(body.contains(NOTE));
    }

    #[test]
    fn a_cut_never_leaves_a_code_block_open_to_swallow_what_follows() {
        let text = format!("```\n{}", "code ".repeat(30_000));
        let body = fit_body("", Some(&text), "### What changed\n", false);
        assert_eq!(body.matches("```").count(), 2, "opened and closed");
        assert!(body.ends_with("### What changed\n"));
    }

    #[test]
    fn repo_urls_come_only_from_the_host_and_the_repo() {
        assert_eq!(
            repo_url("https://github.com", "acme/app"),
            "https://github.com/acme/app.git"
        );
        assert_eq!(
            repo_url("https://github.com/", "acme/app"),
            "https://github.com/acme/app.git"
        );
    }
}
