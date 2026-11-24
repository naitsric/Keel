//! What Keel tells a person while they're not looking at it: a run that waits for them, one that
//! ended, what became of a pull request, and Todo pausing. Only what changed since the last look
//! counts, so nothing is said twice, and what was already so when Keel opened is never said.
//! Sending it is the app's: this only decides what to say.

use std::collections::HashMap;

use keel_core::{AttemptState, CiState, PrState, Reason};
use keel_store::RunSignal;
use serde::{Deserialize, Serialize};

use crate::{Engine, Result, locked};

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

/// What a run was the last time Keel looked.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    attempt_id: String,
    state: Option<AttemptState>,
    finished: bool,
    pr_state: Option<PrState>,
    pr_ci: Option<CiState>,
}

impl From<&RunSignal> for Seen {
    fn from(run: &RunSignal) -> Self {
        Self {
            attempt_id: run.attempt_id.clone(),
            state: run.state,
            finished: run.finished,
            pr_state: run.pr_state,
            pr_ci: run.pr_ci,
        }
    }
}

/// Remembers what was already said. The first look only takes note.
#[derive(Debug, Default)]
pub struct NoticeWatch {
    runs: HashMap<String, Seen>,
    paused: bool,
    primed: bool,
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
        let paused = locked(&engine.inner.board_paused).clone();
        Ok(self.compare(&runs, paused.as_ref(), engine.notice_settings(), language))
    }

    fn compare(
        &mut self,
        runs: &[RunSignal],
        paused: Option<&Reason>,
        settings: NoticeSettings,
        language: &str,
    ) -> Vec<Notice> {
        let es = language == "es";
        let mut notices = Vec::new();
        let mut seen = HashMap::with_capacity(runs.len());
        for run in runs {
            let now = Seen::from(run);
            // A run Keel hasn't seen yet was nothing before, unless this is the first look.
            let before = self.runs.remove(&run.id);
            if self.primed && before.as_ref() != Some(&now) {
                let said = |text: &str| Notice {
                    run_id: Some(run.id.clone()),
                    title: text.to_owned(),
                    body: format!("{} · {}", run.title, run.project_name),
                };
                let before = before.as_ref();
                if settings.waiting
                    && let Some(text) = waiting_text(now.state, es)
                    && before
                        .is_none_or(|b| (&b.attempt_id, b.state) != (&now.attempt_id, now.state))
                {
                    notices.push(said(text));
                }
                if settings.finished
                    && now.finished
                    && matches!(
                        now.state,
                        Some(AttemptState::Succeeded | AttemptState::Skipped)
                    )
                    && !before.is_some_and(|b| b.finished)
                {
                    notices.push(said(if es { "Terminó" } else { "Done" }));
                }
                if settings.pull_requests {
                    if let Some(text) = pr_text(now.pr_state, es)
                        && before.is_none_or(|b| b.pr_state != now.pr_state)
                    {
                        notices.push(said(text));
                    }
                    let open = matches!(now.pr_state, Some(PrState::Open | PrState::Draft));
                    if open
                        && now.pr_ci == Some(CiState::Failed)
                        && before.is_none_or(|b| b.pr_ci != Some(CiState::Failed))
                    {
                        notices.push(said(if es { "CI en rojo" } else { "CI failed" }));
                    }
                }
            }
            seen.insert(run.id.clone(), now);
        }
        // Runs that dropped out of the newest ones are forgotten.
        self.runs = seen;

        if let Some(reason) = paused
            && self.primed
            && !self.paused
            && settings.paused
        {
            notices.push(paused_notice(reason, es));
        }
        self.paused = paused.is_some();
        self.primed = true;
        notices
    }
}

/// The board's own words for a run that waits for the person. None while it doesn't.
fn waiting_text(state: Option<AttemptState>, es: bool) -> Option<&'static str> {
    Some(match (state?, es) {
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

    fn run(id: &str, attempt: &str, state: AttemptState) -> RunSignal {
        RunSignal {
            id: id.into(),
            title: format!("Run {id}"),
            project_name: "app".into(),
            attempt_id: attempt.into(),
            state: Some(state),
            finished: false,
            pr_state: None,
            pr_ci: None,
        }
    }

    fn primed(runs: &[RunSignal]) -> NoticeWatch {
        let mut watch = NoticeWatch::default();
        assert!(
            watch
                .compare(runs, None, NoticeSettings::default(), "en")
                .is_empty()
        );
        watch
    }

    fn titles(notices: &[Notice]) -> Vec<&str> {
        notices.iter().map(|n| n.title.as_str()).collect()
    }

    #[test]
    fn what_was_already_so_when_keel_opened_is_never_said() {
        let mut watch = NoticeWatch::default();
        let waiting = [run("r1", "a1", AttemptState::WaitingApproval)];
        let limit = Reason::RateLimited {
            resets_at: None,
            continues_at: None,
        };
        assert!(
            watch
                .compare(&waiting, Some(&limit), NoticeSettings::default(), "en")
                .is_empty()
        );
        // Nor again on the next look, since nothing changed.
        assert!(
            watch
                .compare(&waiting, Some(&limit), NoticeSettings::default(), "en")
                .is_empty()
        );
    }

    #[test]
    fn a_run_that_comes_to_wait_for_the_person_is_said_once_with_its_title_and_project() {
        let mut watch = primed(&[run("r1", "a1", AttemptState::Running)]);
        let waiting = [run("r1", "a2", AttemptState::WaitingApproval)];
        let notices = watch.compare(&waiting, None, NoticeSettings::default(), "es");
        assert_eq!(
            notices,
            [Notice {
                run_id: Some("r1".into()),
                title: "Falta tu aprobación".into(),
                body: "Run r1 · app".into()
            }]
        );
        assert!(
            watch
                .compare(&waiting, None, NoticeSettings::default(), "es")
                .is_empty()
        );

        // Sent back, and waiting again in the next round, it's said again.
        let again = [run("r1", "a5", AttemptState::WaitingApproval)];
        assert_eq!(
            titles(&watch.compare(&again, None, NoticeSettings::default(), "en")),
            ["Needs your approval"]
        );
    }

    #[test]
    fn a_run_that_ends_with_what_it_was_asked_for_is_said_but_not_one_a_person_stopped() {
        let mut watch = primed(&[
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
        let notices = watch.compare(&[done, stopped], None, NoticeSettings::default(), "en");
        assert_eq!(titles(&notices), ["Done"]);
        assert_eq!(notices[0].run_id.as_deref(), Some("r1"));
    }

    #[test]
    fn a_pull_request_merged_closed_or_with_ci_failing_is_said() {
        let opened = |ci| RunSignal {
            finished: true,
            pr_state: Some(PrState::Open),
            pr_ci: ci,
            ..run("r1", "a1", AttemptState::Succeeded)
        };
        let mut watch = primed(&[opened(Some(CiState::Pending))]);
        assert_eq!(
            titles(&watch.compare(
                &[opened(Some(CiState::Failed))],
                None,
                NoticeSettings::default(),
                "en"
            )),
            ["CI failed"]
        );
        let merged = RunSignal {
            pr_state: Some(PrState::Merged),
            ..opened(Some(CiState::Failed))
        };
        assert_eq!(
            titles(&watch.compare(&[merged], None, NoticeSettings::default(), "es")),
            ["Pull request mergeado"]
        );
    }

    #[test]
    fn todo_pausing_is_said_when_it_starts_and_not_while_it_lasts() {
        let mut watch = primed(&[]);
        let limit = Reason::RateLimited {
            resets_at: None,
            continues_at: None,
        };
        let notices = watch.compare(&[], Some(&limit), NoticeSettings::default(), "en");
        assert_eq!(
            notices,
            [Notice {
                run_id: None,
                title: "Todo is paused".into(),
                body: "You reached your plan's usage limit. Nothing starts until it resets.".into()
            }]
        );
        assert!(
            watch
                .compare(&[], Some(&limit), NoticeSettings::default(), "en")
                .is_empty()
        );
        assert!(
            watch
                .compare(&[], None, NoticeSettings::default(), "en")
                .is_empty()
        );
        assert_eq!(
            watch
                .compare(&[], Some(&limit), NoticeSettings::default(), "en")
                .len(),
            1
        );
    }

    #[test]
    fn a_kind_turned_off_says_nothing_and_isnt_said_later_when_turned_back_on() {
        let off = NoticeSettings {
            waiting: false,
            ..NoticeSettings::default()
        };
        let mut watch = primed(&[run("r1", "a1", AttemptState::Running)]);
        let waiting = [run("r1", "a2", AttemptState::WaitingAnswers)];
        assert!(watch.compare(&waiting, None, off, "en").is_empty());
        assert!(
            watch
                .compare(&waiting, None, NoticeSettings::default(), "en")
                .is_empty()
        );
    }

    #[test]
    fn a_new_run_already_waiting_is_said() {
        let mut watch = primed(&[]);
        let notices = watch.compare(
            &[run("r9", "z1", AttemptState::Blocked)],
            None,
            NoticeSettings::default(),
            "en",
        );
        assert_eq!(titles(&notices), ["Blocked"]);
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
}
