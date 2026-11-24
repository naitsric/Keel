//! What Keel tells a person while they're not looking at it: a run that waits for them, one that
//! ended, what became of a pull request, and Todo pausing. Only what changed since the last look
//! counts, so nothing is said twice, and what was already so when Keel opened is never said.
//! Sending it is the app's: this only decides what to say.

use std::collections::HashMap;
use std::mem::{Discriminant, discriminant};

use keel_core::{AttemptState, CiState, PrState, Reason, mask_secrets};
use keel_store::{RunSignal, WaitingStep};
use serde::{Deserialize, Serialize};

use crate::board::TodoPause;
use crate::{Engine, Result, brain, is_trial, locked};

const SETTINGS_KEY: &str = "notices";
/// The same runs the home page lists.
const WATCHED: i64 = 200;

/// Which kinds of notice a person wants. All of them until they say otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NoticeSettings {
    /// A run waits for an approval or an answer, or is blocked, failed, stuck or interrupted.
    pub waiting: bool,
    /// A run ended with what it was asked for.
    pub finished: bool,
    /// A run's pull request was merged or closed, or its CI failed.
    pub pull_requests: bool,
    /// Todo stopped starting cards.
    pub paused: bool,
}

impl Default for NoticeSettings {
    fn default() -> Self {
        Self {
            waiting: true,
            finished: true,
            pull_requests: true,
            paused: true,
        }
    }
}

/// One notice, in the person's language. `run_id` is what opening it shows; None for the board.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub run_id: Option<String>,
    pub title: String,
    pub body: String,
}

/// Which of `NoticeSettings` a notice falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Waiting,
    Finished,
    PullRequest,
    Paused,
}

impl NoticeSettings {
    fn wants(&self, kind: Kind) -> bool {
        match kind {
            Kind::Waiting => self.waiting,
            Kind::Finished => self.finished,
            Kind::PullRequest => self.pull_requests,
            Kind::Paused => self.paused,
        }
    }
}

#[derive(Debug)]
struct Said {
    kind: Kind,
    notice: Notice,
}

/// What a run was the last time Keel looked.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    /// The steps that waited for the person, and how each did.
    waiting: Vec<(String, AttemptState)>,
    /// Whether it ended with what it was asked for.
    done: bool,
    pr_state: Option<PrState>,
    /// What its pull request's CI last said, and of which commit. An answer that doesn't say what
    /// the CI says leaves what was known: it's no news about it.
    ci: Option<(CiState, Option<String>)>,
}

impl Seen {
    fn new(run: &RunSignal, before: Option<&Self>) -> Self {
        Self {
            waiting: run
                .waiting
                .iter()
                .filter(|step| needs_the_person(step))
                .map(|step| (step.attempt_id.clone(), step.state))
                .collect(),
            // One that only follows its pull request has done what it was asked for. Ending later, as
            // its pull request is merged, is the pull request's news.
            done: run.state == Some(AttemptState::Watching)
                || (matches!(run.pr_state, Some(PrState::Merged | PrState::Closed))
                    && before.is_some_and(|b| b.done))
                || (run.finished
                    && matches!(
                        run.state,
                        Some(AttemptState::Succeeded | AttemptState::Skipped)
                    )),
            pr_state: run.pr_state,
            ci: match run.pr_ci {
                Some(ci) => Some((ci, run.pr_head.clone())),
                None => before.and_then(|b| b.ci.clone()),
            },
        }
    }

    /// Whether its CI failed, if that isn't what it already was in `before`. A commit that isn't
    /// known yet is no other commit.
    fn ci_just_failed(&self, before: Option<&Self>) -> bool {
        let Some((CiState::Failed, head)) = &self.ci else {
            return false;
        };
        let already = before.and_then(|b| b.ci.as_ref()).is_some_and(|(ci, was)| {
            *ci == CiState::Failed && (was.is_none() || head.is_none() || was == head)
        });
        !already
    }
}

/// A usage limit that says when it resets needs nobody: Keel continues that step by itself.
fn needs_the_person(step: &WaitingStep) -> bool {
    !matches!(
        step.reason,
        Some(Reason::RateLimited {
            continues_at: Some(_),
            ..
        })
    )
}

/// Remembers what was already said. The first look only takes note.
#[derive(Debug, Default)]
pub struct NoticeWatch {
    runs: HashMap<String, Seen>,
    primed: bool,
    /// What was stopping Todo, and None while nothing was. None of either until the scheduler has
    /// looked, which can be after the first look here.
    paused: Option<Option<Discriminant<Reason>>>,
}

impl Engine {
    pub fn notice_settings(&self) -> NoticeSettings {
        self.inner
            .store
            .setting(SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn set_notice_settings(&self, settings: &NoticeSettings) -> Result<()> {
        let text = serde_json::to_string(settings).unwrap_or_default();
        Ok(self.inner.store.set_setting(SETTINGS_KEY, &text)?)
    }
}

impl NoticeWatch {
    /// What changed since the last look that the person wants to hear about, in `language`.
    pub fn look(&mut self, engine: &Engine, language: &str) -> Result<Vec<Notice>> {
        let runs = engine.inner.store.run_signals(WATCHED)?;
        let pause = locked(&engine.inner.todo_pause).clone();
        let said = self.compare(&runs, &pause, engine.notice_settings(), language);
        Ok(said
            .into_iter()
            .filter(|said| engine.is_for_the_person(said))
            .map(|said| said.notice)
            .collect())
    }

    fn compare(
        &mut self,
        runs: &[RunSignal],
        pause: &TodoPause,
        settings: NoticeSettings,
        language: &str,
    ) -> Vec<Said> {
        let es = language == "es";
        let mut said = Vec::new();
        let mut seen = HashMap::with_capacity(runs.len());
        for run in runs {
            // A run Keel hasn't seen yet was nothing before, unless this is the first look.
            let before = self.runs.remove(&run.id);
            let now = Seen::new(run, before.as_ref());
            if self.primed && before.as_ref() != Some(&now) {
                // Shown where anyone passing by can read it, so masked as everywhere else.
                let body = || {
                    format!(
                        "{} · {}",
                        mask_secrets(&run.title),
                        mask_secrets(&run.project_name)
                    )
                };
                for (kind, title) in changes(before.as_ref(), &now, es) {
                    said.push(Said {
                        kind,
                        notice: Notice {
                            run_id: Some(run.id.clone()),
                            title: title.to_owned(),
                            body: body(),
                        },
                    });
                }
            }
            seen.insert(run.id.clone(), now);
        }
        // Runs that dropped out of the newest ones are forgotten.
        self.runs = seen;
        self.primed = true;

        let reason = match pause {
            TodoPause::NotLooked => None,
            TodoPause::Clear => Some(None),
            TodoPause::OnRecord(reason) | TodoPause::Session(reason) => Some(Some(reason)),
        };
        if let Some(reason) = reason {
            let now = reason.map(discriminant);
            if let (Some(reason), Some(before)) = (reason, self.paused)
                && before != now
            {
                said.push(Said {
                    kind: Kind::Paused,
                    notice: paused_notice(reason, es),
                });
            }
            self.paused = Some(now);
        }

        said.retain(|said| settings.wants(said.kind));
        said
    }
}

impl Engine {
    /// Someone trying a command is looking at what it says, and a map is Keel's own reading: neither
    /// ending is news. A map that fails does wait for the person, as any run does.
    fn is_for_the_person(&self, said: &Said) -> bool {
        let Some(run_id) = &said.notice.run_id else {
            return true;
        };
        let Ok(run) = self.inner.store.run(run_id) else {
            return true;
        };
        match said.kind {
            Kind::Waiting => !is_trial(&run),
            Kind::Finished => !is_trial(&run) && !brain::is_map(&run),
            Kind::PullRequest | Kind::Paused => true,
        }
    }
}

/// What a run has to say, having been `before`.
fn changes(before: Option<&Seen>, now: &Seen, es: bool) -> Vec<(Kind, &'static str)> {
    let mut said = Vec::new();
    for step in &now.waiting {
        if let Some(text) = waiting_text(step.1, es)
            && before.is_none_or(|b| !b.waiting.contains(step))
            // Steps that came to the same at once are one thing to say.
            && !said.contains(&(Kind::Waiting, text))
        {
            said.push((Kind::Waiting, text));
        }
    }
    if now.done && !before.is_some_and(|b| b.done) {
        said.push((Kind::Finished, if es { "Terminó" } else { "Done" }));
    }
    if let Some(text) = pr_text(now.pr_state, es)
        && before.is_none_or(|b| b.pr_state != now.pr_state)
    {
        said.push((Kind::PullRequest, text));
    }
    let open = matches!(now.pr_state, Some(PrState::Open | PrState::Draft));
    if open && now.ci_just_failed(before) {
        said.push((
            Kind::PullRequest,
            if es { "CI en rojo" } else { "CI failed" },
        ));
    }
    said
}

/// The board's own words for a step that waits for the person. None if it doesn't.
fn waiting_text(state: AttemptState, es: bool) -> Option<&'static str> {
    Some(match (state, es) {
        (AttemptState::WaitingApproval, true) => "Falta tu aprobación",
        (AttemptState::WaitingApproval, false) => "Needs your approval",
        (AttemptState::WaitingAnswers, true) => "Falta tu decisión",
        (AttemptState::WaitingAnswers, false) => "Needs your decision",
        (AttemptState::Blocked, true) => "Bloqueada",
        (AttemptState::Blocked, false) => "Blocked",
        (AttemptState::Failed, true) => "Falló",
        (AttemptState::Failed, false) => "Failed",
        (AttemptState::Stalled, true) => "Parece atascada",
        (AttemptState::Stalled, false) => "Looks stuck",
        (AttemptState::Interrupted, true) => "Interrumpida",
        (AttemptState::Interrupted, false) => "Interrupted",
        _ => return None,
    })
}

/// Only how a pull request ends. Opening one comes with the run ending.
fn pr_text(state: Option<PrState>, es: bool) -> Option<&'static str> {
    Some(match (state?, es) {
        (PrState::Merged, true) => "Pull request mergeado",
        (PrState::Merged, false) => "Pull request merged",
        (PrState::Closed, true) => "Pull request cerrado sin mergear",
        (PrState::Closed, false) => "Pull request closed without merging",
        _ => return None,
    })
}

fn paused_notice(reason: &Reason, es: bool) -> Notice {
    let limit = matches!(reason, Reason::RateLimited { .. });
    let body = match (limit, es) {
        (true, true) => "Llegaste al límite de uso de tu plan. Nada empieza hasta que se renueve.",
        (true, false) => "You reached your plan's usage limit. Nothing starts until it resets.",
        (false, true) => "Nada empieza hasta que se resuelva. Abre Keel para ver por qué.",
        (false, false) => "Nothing starts until it's fixed. Open Keel to see why.",
    };
    Notice {
        run_id: None,
        title: (if es {
            "Por hacer está en pausa"
        } else {
            "Todo is paused"
        })
        .to_owned(),
        body: body.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run whose only step that counts is `attempt`.
    fn run(id: &str, attempt: &str, state: AttemptState) -> RunSignal {
        let waiting = waiting_text(state, false).map(|_| step(attempt, state));
        RunSignal {
            id: id.into(),
            title: format!("Run {id}"),
            project_name: "app".into(),
            state: Some(state),
            finished: false,
            waiting: waiting.into_iter().collect(),
            pr_state: None,
            pr_ci: None,
            pr_head: None,
        }
    }

    fn step(attempt: &str, state: AttemptState) -> WaitingStep {
        WaitingStep {
            attempt_id: attempt.into(),
            state,
            reason: None,
        }
    }

    /// An ended run with its pull request open, and what its CI says of which commit.
    fn opened(ci: Option<CiState>, head: &str) -> RunSignal {
        RunSignal {
            finished: true,
            pr_state: Some(PrState::Open),
            pr_ci: ci,
            pr_head: Some(head.into()),
            ..run("r1", "a1", AttemptState::Succeeded)
        }
    }

    const LIMIT: Reason = Reason::RateLimited {
        resets_at: None,
        continues_at: None,
    };

    /// A watch, and what it says of what it's shown, in English and with every kind on.
    struct Watch(NoticeWatch);

    impl Watch {
        /// One that has already looked, with Todo free to start.
        fn primed(runs: &[RunSignal]) -> Self {
            let mut watch = Self(NoticeWatch::default());
            assert_eq!(watch.runs(runs), [""; 0]);
            watch
        }

        fn say(&mut self, runs: &[RunSignal], pause: &TodoPause) -> Vec<Notice> {
            self.0
                .compare(runs, pause, NoticeSettings::default(), "en")
                .into_iter()
                .map(|said| said.notice)
                .collect()
        }

        fn runs(&mut self, runs: &[RunSignal]) -> Vec<String> {
            titles(self.say(runs, &TodoPause::Clear))
        }

        fn pause(&mut self, pause: &TodoPause) -> Vec<String> {
            titles(self.say(&[], pause))
        }
    }

    fn titles(notices: Vec<Notice>) -> Vec<String> {
        notices.into_iter().map(|n| n.title).collect()
    }

    #[test]
    fn what_was_already_so_when_keel_opened_is_never_said() {
        let mut watch = Watch(NoticeWatch::default());
        let waiting = [run("r1", "a1", AttemptState::WaitingApproval)];
        let limit = TodoPause::OnRecord(LIMIT);
        assert_eq!(watch.say(&waiting, &limit), []);
        // Nor again on the next look, since nothing changed.
        assert_eq!(watch.say(&waiting, &limit), []);
    }

    #[test]
    fn a_run_that_comes_to_wait_for_the_person_is_said_once_with_its_title_and_project() {
        let mut watch = Watch::primed(&[run("r1", "a1", AttemptState::Running)]);
        let waiting = [run("r1", "a2", AttemptState::WaitingApproval)];
        let said = watch
            .0
            .compare(&waiting, &TodoPause::Clear, NoticeSettings::default(), "es");
        assert_eq!(said.len(), 1);
        assert_eq!(
            said[0].notice,
            Notice {
                run_id: Some("r1".into()),
                title: "Falta tu aprobación".into(),
                body: "Run r1 · app".into()
            }
        );
        assert_eq!(watch.runs(&waiting), [""; 0]);

        // Sent back, and waiting again in the next round, it's said again.
        let again = [run("r1", "a5", AttemptState::WaitingApproval)];
        assert_eq!(watch.runs(&again), ["Needs your approval"]);
    }

    #[test]
    fn a_secret_in_a_title_is_masked_as_it_is_on_screen() {
        let mut watch = Watch::primed(&[run("r1", "a1", AttemptState::Running)]);
        let waiting = RunSignal {
            title: "Use API_KEY=made-up-Value-123 to test".into(),
            ..run("r1", "a2", AttemptState::WaitingApproval)
        };
        let body = &watch.say(&[waiting], &TodoPause::Clear)[0].body;
        assert!(!body.contains("made-up-Value-123"), "{body}");
        assert_eq!(
            *body,
            format!("Use API_KEY={} to test · app", keel_core::MASK)
        );
    }

    #[test]
    fn steps_that_run_at_once_are_each_said_once_however_the_others_go() {
        let reviews = |steps: &[(&str, AttemptState)]| RunSignal {
            waiting: steps.iter().map(|(id, state)| step(id, *state)).collect(),
            ..run("r1", "a1", AttemptState::Running)
        };
        use AttemptState::{Blocked, Failed};
        let mut watch = Watch::primed(&[reviews(&[])]);
        assert_eq!(watch.runs(&[reviews(&[("a", Failed)])]), ["Failed"]);
        assert_eq!(
            watch.runs(&[reviews(&[("a", Failed), ("b", Failed)])]),
            ["Failed"]
        );
        // The person tries one again. The other still waits, and that's no news.
        assert_eq!(watch.runs(&[reviews(&[("a", Failed)])]), [""; 0]);
        // An older step that comes to wait is said, whatever a newer one does.
        assert_eq!(
            watch.runs(&[reviews(&[("a", Failed), ("c", Blocked)])]),
            ["Blocked"]
        );
        assert_eq!(watch.runs(&[reviews(&[])]), [""; 0]);
        // Two that fail at once are one thing to say.
        assert_eq!(
            watch.runs(&[reviews(&[("d", Failed), ("e", Failed)])]),
            ["Failed"]
        );
    }

    #[test]
    fn a_step_keel_continues_by_itself_waits_for_nobody_until_it_gives_up() {
        let limited = |continues_at| RunSignal {
            waiting: vec![WaitingStep {
                reason: Some(Reason::RateLimited {
                    resets_at: Some(5),
                    continues_at,
                }),
                ..step("a1", AttemptState::Blocked)
            }],
            ..run("r1", "a1", AttemptState::Blocked)
        };
        let mut watch = Watch::primed(&[run("r1", "a1", AttemptState::Running)]);
        assert_eq!(watch.runs(&[limited(Some(65))]), [""; 0]);
        assert_eq!(watch.runs(&[limited(None)]), ["Blocked"]);
        assert_eq!(watch.runs(&[limited(None)]), [""; 0]);
    }

    #[test]
    fn a_run_that_ends_with_what_it_was_asked_for_is_said_but_not_one_a_person_stopped() {
        let mut watch = Watch::primed(&[
            run("r1", "a1", AttemptState::Running),
            run("r2", "b1", AttemptState::Running),
        ]);
        let done = RunSignal {
            finished: true,
            ..run("r1", "a2", AttemptState::Succeeded)
        };
        let stopped = RunSignal {
            finished: true,
            ..run("r2", "b1", AttemptState::Stopped)
        };
        let notices = watch.say(&[done, stopped], &TodoPause::Clear);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].title, "Done");
        assert_eq!(notices[0].run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_pull_request_merged_closed_or_with_ci_failing_is_said() {
        let mut watch = Watch::primed(&[opened(Some(CiState::Pending), "c1")]);
        let failed = || opened(Some(CiState::Failed), "c1");
        assert_eq!(watch.runs(&[failed()]), ["CI failed"]);
        let merged = || RunSignal {
            pr_state: Some(PrState::Merged),
            ..failed()
        };
        assert_eq!(watch.runs(&[merged()]), ["Pull request merged"]);
        assert_eq!(watch.runs(&[merged()]), [""; 0]);

        let mut watch = Watch::primed(&[failed()]);
        let closed = RunSignal {
            pr_state: Some(PrState::Closed),
            ..failed()
        };
        assert_eq!(
            watch.runs(&[closed]),
            ["Pull request closed without merging"]
        );
    }

    #[test]
    fn ci_that_fails_is_said_once_for_each_commit() {
        let mut watch = Watch::primed(&[opened(Some(CiState::Pending), "c1")]);
        let failed = |head| [opened(Some(CiState::Failed), head)];
        assert_eq!(watch.runs(&failed("c1")), ["CI failed"]);
        // An answer that doesn't say what the CI says isn't the failure going away.
        assert_eq!(watch.runs(&[opened(None, "c1")]), [""; 0]);
        assert_eq!(watch.runs(&failed("c1")), [""; 0]);
        // Nor is one that doesn't say which commit it's about another failure.
        let unknown = RunSignal {
            pr_head: None,
            ..failed("c1")[0].clone()
        };
        assert_eq!(watch.runs(&[unknown]), [""; 0]);
        assert_eq!(watch.runs(&failed("c1")), [""; 0]);
        // Another commit's failure is another one, even if Keel never saw it pending.
        assert_eq!(watch.runs(&[opened(None, "c2")]), [""; 0]);
        assert_eq!(watch.runs(&failed("c2")), ["CI failed"]);
        // And so is one that ran again.
        assert_eq!(watch.runs(&[opened(Some(CiState::Pending), "c2")]), [""; 0]);
        assert_eq!(watch.runs(&failed("c2")), ["CI failed"]);
    }

    #[test]
    fn todo_pausing_is_said_when_it_starts_and_not_while_it_lasts() {
        let mut watch = Watch::primed(&[]);
        let limit = TodoPause::OnRecord(LIMIT);
        assert_eq!(
            watch.say(&[], &limit),
            [Notice {
                run_id: None,
                title: "Todo is paused".into(),
                body: "You reached your plan's usage limit. Nothing starts until it resets.".into()
            }]
        );
        assert_eq!(watch.pause(&limit), [""; 0]);
        assert_eq!(watch.pause(&TodoPause::Clear), [""; 0]);
        assert_eq!(watch.pause(&limit), ["Todo is paused"]);
    }

    #[test]
    fn a_pause_the_scheduler_finds_after_keel_opened_was_already_so() {
        let mut watch = Watch(NoticeWatch::default());
        let out = TodoPause::Session(Reason::ClaudeLoggedOut);
        // The scheduler's first look can take longer than the first look here.
        assert_eq!(watch.pause(&TodoPause::NotLooked), [""; 0]);
        assert_eq!(watch.pause(&out), [""; 0]);
        assert_eq!(watch.pause(&TodoPause::Clear), [""; 0]);
        assert_eq!(watch.pause(&out), ["Todo is paused"]);
    }

    #[test]
    fn a_pause_for_something_else_is_said_again() {
        let mut watch = Watch::primed(&[]);
        let out = TodoPause::Session(Reason::ClaudeLoggedOut);
        assert_eq!(watch.pause(&out), ["Todo is paused"]);
        assert_eq!(watch.pause(&TodoPause::OnRecord(LIMIT)), ["Todo is paused"]);
        // Only what it's about: when the limit resets isn't news.
        let resets = TodoPause::OnRecord(Reason::RateLimited {
            resets_at: Some(5),
            continues_at: Some(65),
        });
        assert_eq!(watch.pause(&resets), [""; 0]);
    }

    #[test]
    fn a_kind_turned_off_says_nothing_and_isnt_said_later_when_turned_back_on() {
        let off = NoticeSettings {
            waiting: false,
            ..NoticeSettings::default()
        };
        let mut watch = Watch::primed(&[run("r1", "a1", AttemptState::Running)]);
        let waiting = [run("r1", "a2", AttemptState::WaitingAnswers)];
        let said = watch.0.compare(&waiting, &TodoPause::Clear, off, "en");
        assert!(said.is_empty());
        assert_eq!(watch.runs(&waiting), [""; 0]);
    }

    #[test]
    fn a_new_run_already_waiting_is_said() {
        let mut watch = Watch::primed(&[]);
        assert_eq!(
            watch.runs(&[run("r9", "z1", AttemptState::Blocked)]),
            ["Blocked"]
        );
    }

    #[test]
    fn settings_missing_a_kind_keep_it_on() {
        let settings: NoticeSettings = serde_json::from_str(r#"{"finished":false}"#).unwrap();
        assert_eq!(
            settings,
            NoticeSettings {
                finished: false,
                ..NoticeSettings::default()
            }
        );
    }

    /// Delivered, with its pull request open, and Keel following it.
    fn followed(pr: PrState) -> RunSignal {
        RunSignal {
            pr_state: Some(pr),
            pr_head: Some("abc".into()),
            ..run("r1", "w1", AttemptState::Watching)
        }
    }

    #[test]
    fn a_run_that_only_follows_its_pull_request_is_done_and_says_so_once() {
        let mut watch = Watch::primed(&[run("r1", "a1", AttemptState::Running)]);
        assert_eq!(watch.runs(&[followed(PrState::Draft)]), ["Done"]);
        assert_eq!(watch.runs(&[followed(PrState::Open)]), [""; 0]);
        // Merged, it goes on to what comes after and ends, which is the pull request's news.
        assert_eq!(
            watch.runs(&[followed(PrState::Merged)]),
            ["Pull request merged"]
        );
        let learning = RunSignal {
            pr_state: Some(PrState::Merged),
            ..run("r1", "l1", AttemptState::Running)
        };
        assert_eq!(watch.runs(&[learning]), [""; 0]);
        let ended = RunSignal {
            finished: true,
            pr_state: Some(PrState::Merged),
            ..run("r1", "l1", AttemptState::Succeeded)
        };
        assert_eq!(watch.runs(&[ended]), [""; 0]);
    }

    #[test]
    fn a_round_its_pull_request_asked_for_is_news_again_once_it_is_delivered() {
        let mut watch = Watch::primed(&[followed(PrState::Open)]);
        let working = RunSignal {
            pr_state: Some(PrState::Open),
            ..run("r1", "b2", AttemptState::Running)
        };
        assert_eq!(watch.runs(&[working]), [""; 0]);
        let asks = RunSignal {
            pr_state: Some(PrState::Open),
            ..run("r1", "h2", AttemptState::WaitingApproval)
        };
        assert_eq!(watch.runs(&[asks]), ["Needs your approval"]);
        assert_eq!(watch.runs(&[followed(PrState::Open)]), ["Done"]);
    }
}
