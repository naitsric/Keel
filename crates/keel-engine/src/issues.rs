//! Watches the issues of each project where a person chose a label. Those open with it come to the
//! board's Backlog, where the person decides when they start, and Keel says on each issue when its
//! run starts and when its pull request opens. An issue closed before its card started takes the
//! card with it. An issue that has been on the board never comes back on its own, so a card the
//! person removed stays removed; one Keel removed because its issue closed comes back if it's
//! opened again.
//!
//! It runs outside any sandbox with the project's own token, like looking at pull requests does,
//! and no agent gets anywhere near it. Nothing here is worth stopping for: what it couldn't do waits
//! for the next look, and the project's page says what went wrong.

use std::sync::Arc;
use std::time::Duration;

use keel_harness::github::{self, Issue};
use keel_store::{IssueTold, NewCard, Project, now};

use crate::{Engine, Error, Inner, Result, locked, new_id};

/// How often Keel looks at the issues of every project that watches them.
pub(crate) const LOOK_AGAIN: Duration = Duration::from_secs(2 * 60);
/// GitHub's own limit on a label's name.
pub const MOST_LABEL_CHARS: usize = 50;

/// What came of the last look at a project's issues, since the app opened.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueWatch {
    /// The label it looked for.
    pub label: String,
    pub at: String,
    /// How many open issues have the label.
    pub open: usize,
    /// Of those, how many this look put on the board.
    pub brought: usize,
    /// How many cards it took off because their issue was closed.
    pub removed: usize,
    /// Why it couldn't look, or something the person should know.
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub problem: Option<IssueWatchProblem>,
    /// The last time saying something on an issue didn't work, until it works again.
    #[serde(serialize_with = "keel_core::serialize_masked")]
    pub comment_failed: Option<CommentFailed>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentFailed {
    pub number: i64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum IssueWatchProblem {
    /// The project's folder isn't a repo on GitHub.
    NoRepo,
    /// The project has no GitHub account chosen.
    NoAccount,
    GhMissing,
    /// The repo has no label by that name, so nothing can have it.
    LabelMissing,
    /// What GitHub or `gh` said.
    Github {
        detail: String,
    },
}

/// A label as a person typed it, ready to save: trimmed, and none if empty. `InvalidState` if
/// GitHub couldn't have a label like it.
pub(crate) fn clean_label(label: Option<&str>) -> Result<Option<String>> {
    let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
        return Ok(None);
    };
    if label.chars().count() > MOST_LABEL_CHARS || label.chars().any(char::is_control) {
        return Err(Error::InvalidState);
    }
    Ok(Some(label.to_owned()))
}

impl Engine {
    /// Starts the thread that looks at the issues of every project that watches them, now and then.
    /// Call it once, after `recover`.
    pub fn start_issue_watch(&self) {
        // It doesn't keep the engine alive, so one that's gone doesn't go on asking GitHub.
        let inner = Arc::downgrade(&self.inner);
        let pause = self.inner.config.issue_look_again;
        std::thread::spawn(move || {
            while let Some(inner) = inner.upgrade() {
                for project in inner.store.projects().unwrap_or_default() {
                    if project.issue_label.is_some() {
                        // One already looking is as fresh as this one would be.
                        inner.look_at_issues(&project, false);
                    }
                }
                drop(inner);
                std::thread::sleep(pause);
            }
        });
    }

    /// Chooses the label whose issues Keel brings to the board, or none to stop watching. Saving
    /// one doesn't look yet: `look_at_issues` does.
    pub fn set_issue_label(&self, project_id: &str, label: Option<&str>) -> Result<Project> {
        let label = clean_label(label)?;
        let before = self.inner.store.project(project_id)?.issue_label;
        self.inner
            .store
            .set_issue_label(project_id, label.as_deref())?;
        if before != label {
            locked(&self.inner.issue_watch).remove(project_id);
        }
        Ok(self.inner.store.project(project_id)?)
    }

    /// Looks at a project's issues now, after any look already underway, and says what came of
    /// it. None if it watches none, or its label changed meanwhile.
    pub fn look_at_issues(&self, project_id: &str) -> Result<Option<IssueWatch>> {
        let project = self.inner.store.project(project_id)?;
        Ok(self.inner.look_at_issues(&project, true))
    }

    /// What came of the last look at a project's issues since the app opened, if any.
    pub fn issue_watch(&self, project_id: &str) -> Option<IssueWatch> {
        let watch = locked(&self.inner.issue_watch).get(project_id).cloned()?;
        Some(self.inner.with_comment_failed(project_id, watch))
    }
}

impl Inner {
    /// One look at a project's issues, one at a time for each: after the one underway if `wait`,
    /// or none at all. None if it didn't look, or its label changed while it looked.
    fn look_at_issues(&self, project: &Project, wait: bool) -> Option<IssueWatch> {
        project.issue_label.as_ref()?;
        let lock = locked(&self.issue_looks)
            .entry(project.id.clone())
            .or_default()
            .clone();
        let _looking = if wait {
            locked(&lock)
        } else {
            lock.try_lock().ok()?
        };
        // As it is now: whoever waited may have changed it.
        let project = self.store.project(&project.id).ok()?;
        let label = project.issue_label.clone()?;
        let mut watch = IssueWatch {
            label: label.clone(),
            at: now(),
            open: 0,
            brought: 0,
            removed: 0,
            problem: None,
            comment_failed: None,
        };
        match self.look_and_bring(&project, &label, &mut watch) {
            Ok(()) => {}
            Err(Some(problem)) => watch.problem = Some(problem),
            // The label changed while it asked GitHub: what it found is about nothing.
            Err(None) => return None,
        }
        {
            let mut watches = locked(&self.issue_watch);
            if self.still_watching(&project.id, &label) {
                watches.insert(project.id.clone(), watch.clone());
            }
        }
        if watch.brought > 0 || watch.removed > 0 {
            self.changed("");
        }
        Some(self.with_comment_failed(&project.id, watch))
    }

    fn still_watching(&self, project_id: &str, label: &str) -> bool {
        self.store
            .project(project_id)
            .is_ok_and(|now| now.issue_label.as_deref() == Some(label))
    }

    /// The watch, with the last comment on one of the project's issues that didn't work.
    fn with_comment_failed(&self, project_id: &str, mut watch: IssueWatch) -> IssueWatch {
        watch.comment_failed = locked(&self.issue_comment_failed).get(project_id).cloned();
        watch
    }

    /// `Err(None)` if the label changed while it asked GitHub, and so it changed nothing.
    fn look_and_bring(
        &self,
        project: &Project,
        label: &str,
        watch: &mut IssueWatch,
    ) -> std::result::Result<(), Option<IssueWatchProblem>> {
        let repo = project
            .github_repo
            .as_deref()
            .ok_or(IssueWatchProblem::NoRepo)?;
        let account = project
            .github_account
            .as_deref()
            .ok_or(IssueWatchProblem::NoAccount)?;
        let gh = self.gh().map_err(|_| IssueWatchProblem::GhMissing)?;
        let not_started = self
            .store
            .issue_cards_not_started(&project.id)
            .map_err(|e| github_problem(e.to_string()))?;
        let look = gh
            .token(account)
            .and_then(|token| gh.look_at_issues(&token, repo, label, &not_started))
            .map_err(|e| github_problem(e.detail))?;
        if !self.still_watching(&project.id, label) {
            return Err(None);
        }
        watch.open = look.open.len();

        for number in &look.closed {
            if self
                .store
                .remove_closed_issue_card(&project.id, *number)
                .unwrap_or(false)
            {
                watch.removed += 1;
            }
        }

        // Oldest first, so the Backlog reads in the order they were written. All of them are the
        // repo's, whatever their URL says after a rename: the question was about it.
        let numbers: Vec<i64> = look.open.iter().rev().map(|i| i.number).collect();
        let unseen = self
            .store
            .unseen_issues(&project.id, &numbers)
            .map_err(|e| github_problem(e.to_string()))?;
        for number in unseen {
            let Some(issue) = look.open.iter().find(|i| i.number == number) else {
                continue;
            };
            if self.bring(project, issue).is_ok() {
                watch.brought += 1;
            }
        }
        if !look.label_exists {
            return Err(Some(IssueWatchProblem::LabelMissing));
        }
        Ok(())
    }

    /// Puts an issue at the bottom of the Backlog, as importing it by hand does.
    fn bring(&self, project: &Project, issue: &Issue) -> Result<String> {
        Ok(self.store.add_card(&NewCard {
            id: new_id(),
            project_id: project.id.clone(),
            title: issue.title.clone(),
            body: String::new(),
            issue: serde_json::to_string(issue).ok(),
            issue_number: Some(issue.number),
            body_by: None,
        })?)
    }

    /// Says on a run's issue that it started, or that its pull request opened, once, if its
    /// project watches issues and the issue is in the project's repo. Returns at once.
    pub(crate) fn tell_issue_soon(self: &Arc<Self>, run_id: &str, what: IssueTold) {
        let inner = self.clone();
        let run_id = run_id.to_owned();
        std::thread::spawn(move || inner.tell_issue(&run_id, what));
    }

    fn tell_issue(&self, run_id: &str, what: IssueTold) {
        let Ok(run) = self.store.run(run_id) else {
            return;
        };
        let Ok(project) = self.store.project(&run.project_id) else {
            return;
        };
        let (Some(_), Some(repo), Some(account)) = (
            &project.issue_label,
            project.github_repo.as_deref(),
            project.github_account.as_deref(),
        ) else {
            return;
        };
        // A part of a change across projects can come from another repo's issue, so only one the
        // URL says is the project's. Keel says nothing on a renamed repo's until its remote is too.
        let Some(number) = run
            .source_issue
            .as_ref()
            .and_then(|issue| issue.get("url")?.as_str())
            .and_then(github::issue_in)
            .filter(|(issue_repo, _)| issue_repo.eq_ignore_ascii_case(repo))
            .map(|(_, number)| number)
        else {
            return;
        };
        let text = match what {
            IssueTold::Started => started_text(&run.language),
            IssueTold::PullRequest => match run.pr_url.as_deref() {
                Some(url) => pull_request_text(&run.language, url),
                None => return,
            },
        };
        // Marked before saying it, so it's said once even if GitHub takes its time. One that
        // fails isn't said again: twice would be worse than never.
        if !self.store.mark_issue_told(run_id, what).unwrap_or(false) {
            return;
        }
        let said = self.gh().map_err(|e| e.to_string()).and_then(|gh| {
            gh.token(account)
                .and_then(|token| gh.comment_on_issue(&token, repo, number, &text))
                .map_err(|e| e.detail)
        });
        let mut failed = locked(&self.issue_comment_failed);
        match said {
            Ok(()) => failed.remove(&project.id),
            Err(detail) => failed.insert(project.id.clone(), CommentFailed { number, detail }),
        };
    }
}

fn github_problem(detail: String) -> IssueWatchProblem {
    IssueWatchProblem::Github { detail }
}

fn spanish(language: &str) -> bool {
    language.starts_with("es")
}

fn started_text(language: &str) -> String {
    if spanish(language) {
        "Keel empezó a trabajar en este issue.".to_owned()
    } else {
        "Keel started working on this issue.".to_owned()
    }
}

fn pull_request_text(language: &str, url: &str) -> String {
    if spanish(language) {
        format!("Keel abrió un pull request para este issue: {url}")
    } else {
        format!("Keel opened a pull request for this issue: {url}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_trimmed_and_an_empty_one_watches_nothing() {
        assert_eq!(clean_label(None).unwrap(), None);
        assert_eq!(clean_label(Some("  ")).unwrap(), None);
        assert_eq!(
            clean_label(Some(" keel ")).unwrap().as_deref(),
            Some("keel")
        );
        assert_eq!(
            clean_label(Some("needs: keel, soon")).unwrap().as_deref(),
            Some("needs: keel, soon")
        );
        assert!(clean_label(Some(&"x".repeat(MOST_LABEL_CHARS + 1))).is_err());
        assert!(clean_label(Some(&"é".repeat(MOST_LABEL_CHARS))).is_ok());
        assert!(clean_label(Some("a\nb")).is_err());
    }

    #[test]
    fn what_keel_says_on_an_issue_is_in_the_runs_language() {
        assert_eq!(started_text("es"), "Keel empezó a trabajar en este issue.");
        assert_eq!(started_text("en"), "Keel started working on this issue.");
        assert!(
            pull_request_text("es-CO", "https://github.com/a/b/pull/3")
                .ends_with(": https://github.com/a/b/pull/3")
        );
    }
}
