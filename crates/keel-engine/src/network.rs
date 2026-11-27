//! The domains a project lets its builder reach, and what Keel does when a step can't go on for
//! lack of one: it waits for the person, who allows it, goes on without it or stops the step. The
//! wait is nobody's rework, so no approver sees it and no round goes by.
//!
//! Keel finds out from what a builder says it lacked, in its structured answer, and from the errors
//! its tools give when Keel itself keeps them offline (see `runner::OFFLINE`), only where they
//! can't mean anything else.

use std::collections::HashSet;

use keel_core::{AttemptState, NeededDomain, Reason, ToolProfile, WorkflowStep};
use keel_store::{Attempt, Event, Retry};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Engine, Error, Inner, Result, failed_checks, is_trial, locked, runner, workflows};

/// The most domains one step asks for at once.
const MOST_NEEDED: usize = 10;

/// How much is kept of what a builder says it needed a domain for.
const MOST_WHY_CHARS: usize = 300;

/// What a builder answers with. Its final message goes inside it, since asking for a structured
/// answer leaves no other place for it.
pub(crate) const BUILDER_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["message","neededDomains"],"properties":{"message":{"type":"string","description":"Your whole final message: everything you were asked to say when you finish, written as you would write it."},"neededDomains":{"type":"array","description":"The domains you could not reach because the network is closed to them, without which you could not finish what you were asked. Empty if you lacked none, or managed without them. The person decides whether to allow each one.","items":{"type":"object","additionalProperties":false,"required":["domain","why"],"properties":{"domain":{"type":"string","description":"The host name alone, such as registry.npmjs.org: no scheme, path, port or wildcard."},"why":{"type":"string","description":"What you needed it for, in one sentence, in the language of your message."}}}}}}"#;

/// What a person says to a step waiting for a domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NetworkAnswer {
    /// The project may reach it from now on, and the step starts over.
    Allow,
    /// The run goes on as it would have without asking.
    GoWithout,
    /// The step stops here, for the person to try again or close the run.
    Stop,
}

/// Whether `domain` is one a project can allow: a host name, with an optional `*.` for its
/// subdomains, and no scheme, path, port or blanks.
pub fn is_domain(domain: &str) -> bool {
    let host = domain.strip_prefix("*.").unwrap_or(domain);
    host.len() <= 253
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// What names of this Mac and of a local network end in. None of them is anyone's on the internet.
const LOCAL_ENDINGS: [&str; 9] = [
    "localhost",
    "local",
    "internal",
    "lan",
    "home",
    "home.arpa",
    "intranet",
    "corp",
    "localdomain",
];

/// Whether a domain, a wildcard included, is a name for a host on the internet: not this Mac, the
/// local network or an IP address, however it's written. A name with no dot is a machine's on the
/// network this Mac is on, such as `router`. An address ends in a number, which may be
/// hexadecimal, and no name's last label does.
pub(crate) fn names_the_internet(domain: &str) -> bool {
    let domain = domain.trim().to_ascii_lowercase();
    let host = domain.strip_prefix("*.").unwrap_or(&domain);
    let last = host.rsplit('.').next().unwrap_or_default();
    let local = LOCAL_ENDINGS.iter().any(|ending| {
        host.strip_suffix(ending)
            .is_some_and(|rest| rest.is_empty() || rest.ends_with('.'))
    });
    host.contains('.')
        && !local
        && !last.starts_with("0x")
        && !last.bytes().all(|b| b.is_ascii_digit())
}

/// Whether what a project allows already lets a step reach this host.
pub(crate) fn covers(allowed: &[String], host: &str) -> bool {
    allowed.iter().any(|domain| {
        let domain = domain.trim().to_ascii_lowercase();
        match domain.strip_prefix("*.") {
            Some(base) => host
                .strip_suffix(base)
                .is_some_and(|sub| sub.ends_with('.')),
            None => domain == host,
        }
    })
}

/// The hosts a known registry's tool downloads from, for the registry's own name or one of those
/// hosts. Any other name stands for itself, a registry's website included: whoever asks for
/// `www.npmjs.com` wants to read a page, and offering them where npm downloads from would open
/// that and leave the page closed.
fn hosts_of(domain: &str) -> Vec<&str> {
    match domain {
        "npmjs.org" => vec![NPM],
        "crates.io" | "index.crates.io" | "static.crates.io" => CARGO.to_vec(),
        "pypi.org" | "files.pythonhosted.org" => PIP.to_vec(),
        "proxy.golang.org" => GO.to_vec(),
        other => vec![other],
    }
}

const NPM: &str = "registry.npmjs.org";
const CARGO: [&str; 2] = ["index.crates.io", "static.crates.io"];
const PIP: [&str; 2] = ["pypi.org", "files.pythonhosted.org"];
const GO: [&str; 2] = ["proxy.golang.org", "sum.golang.org"];

/// Whether a host is one a known registry's tool downloads from, under another name than the
/// registry's: see `runner::OFFLINE`.
pub(crate) fn is_registry_download(host: &str) -> bool {
    PIP.contains(&host)
}

/// What a builder says it lacked. Only one host at a time, on the internet: an agent asks for what
/// it needed, and never for a wildcard or for this Mac.
fn declared(answer: &Value) -> Vec<NeededDomain> {
    let items = answer["neededDomains"].as_array();
    items
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let domain = item["domain"].as_str()?.trim().to_ascii_lowercase();
            if domain.starts_with("*.") || !is_domain(&domain) || !names_the_internet(&domain) {
                return None;
            }
            let why: String = item["why"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(MOST_WHY_CHARS)
                .collect();
            Some(
                hosts_of(&domain)
                    .into_iter()
                    .map(|host| NeededDomain {
                        domain: host.to_owned(),
                        why: (!why.is_empty()).then(|| why.clone()),
                        tool: None,
                        frees_offline: false,
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

/// A tool Keel can tell failed because Keel keeps it offline.
struct Offline {
    tool: &'static str,
    /// The domain that, allowed to the builder, lets the tool go online: see `runner::OFFLINE`.
    registry: &'static str,
    hosts: &'static [&'static str],
}

/// A line without the blanks and the bracket some tools put before an error's code.
fn code_line(line: &str) -> &str {
    line.trim_start().trim_start_matches('[')
}

/// Whether every address a line names is on `host`. One that names another registry says something
/// else is missing, and which is not for Keel to guess.
fn only_names(output: &str, marker: &str, host: &str) -> bool {
    output
        .lines()
        .filter_map(|line| line.split_once(marker))
        .all(|(_, address)| {
            address
                .strip_prefix(host)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(['/', ' ']))
        })
}

fn npm_failed_offline(output: &str) -> bool {
    output.lines().any(|line| {
        ["npm error code ENOTCACHED", "npm ERR! code ENOTCACHED"].contains(&line.trim())
    }) && only_names(output, "request to https://", NPM)
}

fn pnpm_failed_offline(output: &str) -> bool {
    output.lines().map(code_line).any(|line| {
        ["ERR_PNPM_NO_OFFLINE_TARBALL", "ERR_PNPM_NO_OFFLINE_META"]
            .iter()
            .any(|code| line.starts_with(code))
    }) && only_names(output, "downloaded from https://", NPM)
}

fn cargo_failed_offline(output: &str) -> bool {
    output.lines().any(|line| {
        line.trim() == "attempting to make an HTTP request, but --offline was specified"
    }) && !output.contains("(registry `")
}

/// The tools whose output says, beyond doubt, that they failed for being offline. pip isn't one:
/// kept from its index it says what it says of any package it can't find, a misspelt one or one
/// of a private index included, and going by that would offer to open PyPI for a package that
/// was never there. What pip lacked is the builder's to say.
fn failed_offline(output: &str) -> Vec<Offline> {
    type Says = fn(&str) -> bool;
    let known: [(Says, Offline); 3] = [
        (
            npm_failed_offline,
            Offline {
                tool: "npm",
                registry: "npmjs.org",
                hosts: &[NPM],
            },
        ),
        (
            pnpm_failed_offline,
            Offline {
                tool: "pnpm",
                registry: "npmjs.org",
                hosts: &[NPM],
            },
        ),
        (
            cargo_failed_offline,
            Offline {
                tool: "cargo",
                registry: "crates.io",
                hosts: &CARGO,
            },
        ),
    ];
    known
        .into_iter()
        .filter(|(says, _)| says(output))
        .map(|(_, offline)| offline)
        .collect()
}

fn seen(offline: &Offline) -> impl Iterator<Item = NeededDomain> {
    offline.hosts.iter().map(|host| NeededDomain {
        domain: (*host).to_owned(),
        why: None,
        tool: Some(offline.tool.to_owned()),
        frees_offline: false,
    })
}

/// What the builder's last command printed, if it failed. One it got past, or went on after, doesn't
/// say it couldn't finish.
fn last_command_failure(events: &[Event]) -> Option<&str> {
    let commands: HashSet<&str> = events
        .iter()
        .filter(|e| e.kind == "toolCall" && e.data["name"] == "Bash")
        .filter_map(|e| e.data["id"].as_str())
        .collect();
    let last = events.iter().rev().find(|e| {
        e.kind == "toolResult"
            && e.data["id"]
                .as_str()
                .is_some_and(|id| commands.contains(id))
    })?;
    (last.data["isError"] == true)
        .then(|| last.data["text"].as_str())
        .flatten()
}

/// Whether this step is one that builds: an agent that works on the code and can run commands.
pub(crate) fn builds(kind: &str, role: &str, tools: &str) -> bool {
    kind == "agent" && role == "worker" && tools == ToolProfile::Build.as_str()
}

/// What a step waits for, if it waits for the person to say whether it may reach a domain.
fn waiting_on(attempt: &Attempt) -> Option<Vec<NeededDomain>> {
    if attempt.state != AttemptState::Blocked.as_str() {
        return None;
    }
    match serde_json::from_value(attempt.reason.clone()?).ok()? {
        Reason::NetworkNeeded { needed } => Some(needed),
        _ => None,
    }
}

impl Inner {
    /// As a step ends, before its state is on the record: the domains it couldn't go on without,
    /// which the project doesn't allow, if that's why it should wait. `state` and `reason` are how
    /// it would end otherwise, and `answer` what an agent answered. Anything that can't be read
    /// leaves it to end as it would, and is written to the log.
    pub(crate) fn lacks_network(
        &self,
        attempt_id: &str,
        state: AttemptState,
        reason: Option<&Reason>,
        answer: Option<&Value>,
    ) -> Option<Vec<NeededDomain>> {
        let found = self.find_lacking(attempt_id, state, reason, answer);
        self.noted("looking for the domains a step lacked", attempt_id, found)
            .filter(|needed| !needed.is_empty())
    }

    fn find_lacking(
        &self,
        attempt_id: &str,
        state: AttemptState,
        reason: Option<&Reason>,
        answer: Option<&Value>,
    ) -> Result<Vec<NeededDomain>> {
        let attempt = self.store.attempt(attempt_id)?;
        let built = builds(&attempt.kind, &attempt.role, &attempt.tools)
            && state == AttemptState::Succeeded;
        let checked = attempt.kind == "check"
            && state == AttemptState::Failed
            && failed_checks::about_the_work(reason);
        if !built && !checked {
            return Ok(Vec::new());
        }
        let run = self.store.run(&attempt.run_id)?;
        // Someone trying a command already has their answer: see `close_failed_trial`.
        if run.finished_at.is_some() || is_trial(&run) {
            return Ok(Vec::new());
        }
        let events = self.store.events(attempt_id, 0)?;
        // What a builder ran with, whatever the project allows by now.
        let had: Vec<String> = serde_json::from_value(
            attempt.settings["sandbox"]["network"]["allowedDomains"].clone(),
        )
        .unwrap_or_default();
        let found: Vec<NeededDomain> = if built {
            let seen = last_command_failure(&events)
                .map(failed_offline)
                .unwrap_or_default()
                .iter()
                .filter(|offline| !runner::allows_host(&had, offline.registry))
                .flat_map(seen)
                .collect::<Vec<_>>();
            answer
                .map(declared)
                .unwrap_or_default()
                .into_iter()
                .chain(seen)
                .collect()
        } else {
            // A check is offline, all but the one repeated after allowing what it lacked. One a
            // person killed didn't fail for that.
            if failed_checks::stopped_from_outside(&events) {
                return Ok(Vec::new());
            }
            let printed = events.iter().rev().find(|e| e.kind == "checkResult");
            let part = |name: &str| {
                printed
                    .and_then(|e| e.data[name].as_str())
                    .unwrap_or_default()
            };
            failed_offline(&format!("{}\n{}", part("stdout"), part("stderr")))
                .iter()
                .flat_map(seen)
                .collect()
        };
        // A builder lacked what it didn't have, even if the project allows it by now. A check has
        // no network of its own, so for one it's what the project doesn't allow.
        let allowed = if built {
            had
        } else {
            self.store.project(&run.project_id)?.allowed_domains
        };
        // What the person already chose to go on without, they aren't asked about again.
        let declined: HashSet<String> = self
            .store
            .run_events(&run.id, "networkDeclined")?
            .iter()
            .filter_map(|e| e.data["domains"].as_array())
            .flatten()
            .filter_map(|domain| domain.as_str().map(str::to_owned))
            .collect();
        let mut needed: Vec<NeededDomain> = Vec::new();
        for domain in found {
            if covers(&allowed, &domain.domain) || declined.contains(&domain.domain) {
                continue;
            }
            // Said by the builder and seen too, it's both what it was for and which tool failed.
            if let Some(there) = needed.iter_mut().find(|n| n.domain == domain.domain) {
                there.why = there.why.take().or(domain.why);
                there.tool = there.tool.take().or(domain.tool);
            } else if needed.len() < MOST_NEEDED {
                needed.push(domain);
            }
        }
        // Said here, for the person to know before they allow it: see `runner::online_after`.
        for domain in &mut needed {
            domain.frees_offline = !runner::is_known_registry(&domain.domain);
        }
        Ok(needed)
    }

    /// As a step ends, before its state is on the record: if it couldn't go on for lack of a
    /// domain (see `lacks_network`), it waits for the person, and `state` and `reason` say so. A
    /// stop asked for while it closed wins: then it ends stopped, and waits for nothing. True if it
    /// waits, which is then on the record already: from there on a person can answer it, so
    /// whoever closes the step doesn't write how it stands again.
    pub(crate) fn waits_for_network(
        &self,
        attempt_id: &str,
        state: &mut AttemptState,
        reason: &mut Option<Reason>,
        answer: Option<&Value>,
    ) -> bool {
        let Some(needed) = self.lacks_network(attempt_id, *state, reason.as_ref(), answer) else {
            return false;
        };
        // A stop is taken with this held, while the attempt still reads as running: see
        // `Engine::stop`. One taken before the wait is written wins, and one after finds the step
        // waiting, which only its own answer stops.
        let controls = locked(&self.controls);
        if controls.get(attempt_id).is_some_and(|c| c.stop_requested) {
            *state = AttemptState::Stopped;
            *reason = None;
            return false;
        }
        *state = AttemptState::Blocked;
        *reason = Some(Reason::NetworkNeeded { needed });
        self.keep_state(attempt_id, *state, reason.as_ref());
        true
    }

    /// What a step lacked, if it waits for a domain and a person allowed it, which starts the step
    /// over.
    pub(crate) fn allowed_wait(&self, attempt_id: &str) -> Result<Option<Vec<NeededDomain>>> {
        let Some(needed) = waiting_on(&self.store.attempt(attempt_id)?) else {
            return Ok(None);
        };
        let allowed = self
            .store
            .events(attempt_id, 0)?
            .iter()
            .any(|event| event.kind == "networkAllowed");
        Ok(allowed.then_some(needed))
    }

    /// What a person allowed `attempt`'s step in its round before it started, one list for each
    /// time it waited. It holds for every later try there, however it came about.
    pub(crate) fn repeated_after_allowing(
        &self,
        attempt: &Attempt,
    ) -> Result<Vec<Vec<NeededDomain>>> {
        let mut allowed = Vec::new();
        for earlier in self.store.attempts_of_run(&attempt.run_id)? {
            if earlier.step_id == attempt.step_id
                && earlier.round == attempt.round
                && earlier.number < attempt.number
                && let Some(needed) = self.allowed_wait(&earlier.id)?
            {
                allowed.push(needed);
            }
        }
        Ok(allowed)
    }

    /// The offline variables `attempt` goes without for what its step was allowed in its round:
    /// see `runner::online_after`.
    pub(crate) fn online_for_step(&self, attempt: &Attempt) -> Result<Vec<String>> {
        Ok(self
            .repeated_after_allowing(attempt)?
            .iter()
            .flat_map(|needed| runner::online_after(needed))
            .map(str::to_owned)
            .collect())
    }

    /// A builder's: those, and the ones freed by a domain that's no known registry, which a person
    /// allowed in any run and the project still allows.
    pub(crate) fn online_for_builder(&self, attempt: &Attempt) -> Result<Vec<String>> {
        let run = self.store.run(&attempt.run_id)?;
        let mut online = self.online_for_step(attempt)?;
        online.extend(self.store.freed_variables(&run.project_id)?);
        Ok(online)
    }

    /// The domains a check of this step reaches when it's tried again in this round with this
    /// `command`, or none. It's the same command a try of it ran when it waited and a person
    /// allowed what it lacked, and no try since got to say anything about the work. It reaches what
    /// those waits allowed and the project still allows, and nothing else the project does.
    /// `allowing` is the wait this try answers, whose domains are on the record only once it's
    /// started: see `Engine::answer_network`.
    pub(crate) fn inherited_network(
        &self,
        check: &CheckTry,
        project_allows: &[String],
        allowing: Option<(&str, &[String])>,
    ) -> Result<Vec<String>> {
        let attempts = self.store.attempts_of_run(check.run_id)?;
        let mut domains: Vec<String> = Vec::new();
        for earlier in attempts
            .iter()
            .rev()
            .filter(|a| a.step_id == check.step_id && a.round == check.round)
        {
            let allowed = match allowing {
                Some((wait, domains)) if wait == earlier.id => Some(domains.to_vec()),
                _ => self
                    .allowed_wait(&earlier.id)?
                    .map(|needed| needed.into_iter().map(|n| n.domain).collect()),
            };
            if let Some(allowed) = allowed {
                // What was allowed was for the command that asked.
                if earlier.prompt.trim() != check.command.trim() {
                    break;
                }
                for domain in allowed {
                    if covers(project_allows, &domain) && !domains.contains(&domain) {
                        domains.push(domain);
                    }
                }
                continue;
            }
            let events = self.store.events(&earlier.id, 0)?;
            if failed_checks::finished_its_work(earlier, &events) {
                break;
            }
        }
        Ok(domains)
    }
}

/// A check about to be tried: its run, step and round, and the command it runs.
pub(crate) struct CheckTry<'a> {
    pub run_id: &'a str,
    pub step_id: &'a str,
    pub round: i64,
    pub command: &'a str,
}

impl Engine {
    /// A person answers a step that waits to know whether it may reach a domain. Only they do: no
    /// MCP tool reaches this. Allowing adds the domains to the project and starts the step over, in
    /// the same round. If it can't start, the domains stay added, the error says so, and the step
    /// still waits. Going without ends the step as it would have ended without asking.
    pub fn answer_network(&self, attempt_id: &str, answer: NetworkAnswer) -> Result<()> {
        self.answer_network_as(attempt_id, answer, None)
    }

    /// Stops a step that waits for a domain, for an MCP client the person lets stop runs. It
    /// answers nothing else: allowing the domain and going on without it stay the person's. Its
    /// event names the client, as `stop_by`'s does.
    pub fn stop_waiting_by(&self, attempt_id: &str, by: &str) -> Result<()> {
        self.answer_network_as(attempt_id, NetworkAnswer::Stop, Some(by))
    }

    fn answer_network_as(
        &self,
        attempt_id: &str,
        answer: NetworkAnswer,
        by: Option<&str>,
    ) -> Result<()> {
        let inner = &self.inner;
        let store = &inner.store;
        // Held from looking until it's on the record, so only one answer goes through.
        let guard = locked(&inner.advancing);
        let attempt = store.attempt(attempt_id)?;
        let needed = waiting_on(&attempt).ok_or(Error::InvalidState)?;
        let run = store.run(&attempt.run_id)?;
        let attempts = store.attempts_of_run(&attempt.run_id)?;
        if run.finished_at.is_some() || !workflows::is_latest_of_its_step(&attempts, &attempt) {
            return Err(Error::InvalidState);
        }
        let domains: Vec<String> = needed.iter().map(|n| n.domain.clone()).collect();
        let finished = |state: AttemptState| {
            store.append_event(attempt_id, "finished", &json!({ "state": state.as_str() }))
        };
        let moves_on = match answer {
            NetworkAnswer::Allow => {
                if store.project_is_removed(&run.project_id)? {
                    return Err(Error::Blocked(Reason::ProjectRemoved));
                }
                if inner.copy_freed(&run) {
                    return Err(Error::Blocked(Reason::CopyFreed));
                }
                let steps: Vec<WorkflowStep> =
                    serde_json::from_value(run.workflow.clone()).unwrap_or_default();
                let step =
                    workflows::find_step(&steps, &attempt.step_id).ok_or(Error::InvalidState)?;
                // Before the step starts, which reads what the project allows.
                store.allow_domains(&run.project_id, &domains, &runner::freed_by(&needed))?;
                // That they were allowed goes on the record with the try that repeats the step,
                // so a step that couldn't start isn't one a person allowed and nothing came of.
                let allowed = json!({ "domains": domains });
                let retry = Retry {
                    run_id: &attempt.run_id,
                    retried: attempt_id,
                    found: AttemptState::Blocked,
                    requested: None,
                    reopens: false,
                    commands: None,
                    put_off: None,
                    allowed: Some(&allowed),
                };
                let number = workflows::next_number(&attempts, &attempt.step_id);
                let started =
                    inner.start_step_with(&attempt.run_id, &step, number, None, Some(&retry), None);
                if let Err(error) = started {
                    // The project has them all the same, which the person has to know.
                    drop(guard);
                    inner.changed("");
                    return Err(Error::Blocked(Reason::DomainAllowedNotStarted {
                        domains,
                        detail: error.to_string(),
                    }));
                }
                false
            }
            NetworkAnswer::GoWithout => {
                // On the record only once the step has gone on: it's what keeps a later step from
                // asking for the same domains, and what the person is shown they chose.
                let declined = || {
                    store.append_event(
                        attempt_id,
                        "networkDeclined",
                        &json!({ "domains": domains }),
                    )
                };
                if attempt.kind == "check" {
                    // The check failed, so now it does what a failed check does.
                    let mut failed = Reason::CheckFailed {
                        exit_code: attempt.exit_code.and_then(|code| i32::try_from(code).ok()),
                        localhost_refused: false,
                        failed_times: None,
                        unchanged: false,
                    };
                    let sent = inner.send_failed_check_back_held(attempt_id, &mut failed)?;
                    if !sent {
                        store.set_state(attempt_id, AttemptState::Failed, Some(&failed))?;
                    }
                    declined()?;
                    finished(if sent {
                        AttemptState::Returned
                    } else {
                        AttemptState::Failed
                    })?;
                    sent
                } else {
                    if !store.change_state(
                        attempt_id,
                        AttemptState::Blocked,
                        AttemptState::Succeeded,
                    )? {
                        return Err(Error::InvalidState);
                    }
                    declined()?;
                    finished(AttemptState::Succeeded)?;
                    true
                }
            }
            NetworkAnswer::Stop => {
                if !store.change_state(attempt_id, AttemptState::Blocked, AttemptState::Stopped)? {
                    return Err(Error::InvalidState);
                }
                let data = match by {
                    Some(by) => json!({ "by": by }),
                    None => json!({}),
                };
                store.append_event(attempt_id, "stopRequested", &data)?;
                finished(AttemptState::Stopped)?;
                false
            }
        };
        drop(guard);
        if moves_on {
            inner.advance(&attempt.run_id)?;
        }
        inner.changed(&attempt.run_id);
        if answer == NetworkAnswer::Allow {
            // The project changed too.
            inner.changed("");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use keel_harness::Tools;
    use keel_store::{NewAttempt, NewRun, Project, ProjectSettings, Store};

    use super::*;
    use crate::{Config, locked};

    fn domains(allowed: &[&str]) -> Vec<String> {
        allowed.iter().map(|d| (*d).to_owned()).collect()
    }

    #[test]
    fn a_domain_is_a_host_name_with_an_optional_wildcard_and_nothing_else() {
        for good in [
            "registry.npmjs.org",
            "PyPI.org",
            "*.example.com",
            "xn--bcher-kva.example",
            "localhost",
            "10.0.0.1",
        ] {
            assert!(is_domain(good), "{good}");
        }
        let long = format!("{}.com", "a".repeat(64));
        let longer = ["a"; 200].join(".");
        for bad in [
            "",
            "*",
            "*.",
            "*.*.com",
            "a.*.com",
            "https://example.com",
            "example.com/",
            "example.com/path",
            "example.com:443",
            "user@example.com",
            "two words",
            " example.com",
            "example.com ",
            "example.com\n",
            "example..com",
            ".example.com",
            "example.com.",
            "-example.com",
            "example-.com",
            "under_score.com",
            "ex\u{430}mple.com",
            long.as_str(),
            longer.as_str(),
        ] {
            assert!(!is_domain(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_builders_answer_asks_for_its_message_and_what_it_lacked() {
        let schema: Value = serde_json::from_str(BUILDER_SCHEMA).unwrap();
        assert_eq!(schema["required"], json!(["message", "neededDomains"]));
    }

    #[test]
    fn a_wildcard_covers_subdomains_and_a_name_only_itself() {
        let allowed = domains(&["*.npmjs.org", "PyPI.org"]);
        for host in ["registry.npmjs.org", "a.b.npmjs.org", "pypi.org"] {
            assert!(covers(&allowed, host), "{host}");
        }
        for host in ["npmjs.org", "evilnpmjs.org", "files.pypi.org", "crates.io"] {
            assert!(!covers(&allowed, host), "{host}");
        }
    }

    #[test]
    fn any_host_of_pip_or_cargo_asks_for_both_their_downloads_need() {
        for named in ["pypi.org", "files.pythonhosted.org"] {
            assert_eq!(hosts_of(named), PIP, "{named}");
        }
        for named in ["crates.io", "index.crates.io", "static.crates.io"] {
            assert_eq!(hosts_of(named), CARGO, "{named}");
        }
        let found = declared(&json!({ "neededDomains": [
            { "domain": "files.pythonhosted.org", "why": "a wheel" },
        ]}));
        let names: Vec<&str> = found.iter().map(|n| n.domain.as_str()).collect();
        assert_eq!(names, PIP);
    }

    /// Whoever asks for a registry's website wants that page, and not where its tool downloads
    /// from.
    #[test]
    fn a_registrys_website_stands_for_itself() {
        for site in [
            "www.npmjs.com",
            "www.npmjs.org",
            "npmjs.com",
            "golang.org",
            "go.dev",
            "maven.apache.org",
            "docs.rs",
        ] {
            assert_eq!(hosts_of(site), [site]);
            let found = declared(&json!({ "neededDomains": [
                { "domain": site, "why": "to read a package's page" },
            ]}));
            let names: Vec<&str> = found.iter().map(|n| n.domain.as_str()).collect();
            assert_eq!(names, [site]);
        }
        // The registry's own name is where its tool downloads from.
        assert_eq!(hosts_of("npmjs.org"), [NPM]);
    }

    /// Go checks what it downloads against another host.
    #[test]
    fn gos_proxy_asks_for_the_host_that_checks_its_downloads_too() {
        assert_eq!(hosts_of("proxy.golang.org"), GO);
        let found = declared(&json!({ "neededDomains": [
            { "domain": "proxy.golang.org", "why": "a module" },
        ]}));
        let names: Vec<&str> = found.iter().map(|n| n.domain.as_str()).collect();
        assert_eq!(names, ["proxy.golang.org", "sum.golang.org"]);
    }

    #[test]
    fn a_builder_never_asks_for_this_mac_the_local_network_or_an_address() {
        for domain in [
            "api.localhost",
            "a.b.LocalHost",
            "printer.local",
            "NAS.Local",
            "127.0.0.1",
            "127.1",
            "foo.123",
            "0x7f.0.0.0x1",
            "127.0.0x1",
            "0x7f.0x1",
            "host.0X7F",
            "router",
            "nas.lan",
            "registry.Internal",
            "a.b.corp",
            "printer.home",
            "printer.home.arpa",
            "wiki.intranet",
            "box.localdomain",
        ] {
            let answer = json!({ "neededDomains": [{ "domain": domain, "why": "x" }] });
            assert!(declared(&answer).is_empty(), "{domain}");
        }
        // Names that only look like those.
        for domain in [
            "localhost.example.com",
            "local.dev",
            "0x.org",
            "a1.example2",
            "internal.example.com",
            "lan.example",
            "mycorp.example",
            "homes.example",
            "x.arpa.example",
        ] {
            let answer = json!({ "neededDomains": [{ "domain": domain, "why": "x" }] });
            assert_eq!(declared(&answer).len(), 1, "{domain}");
        }
    }

    #[test]
    fn a_name_of_a_local_network_is_not_one_on_the_internet_with_a_wildcard_or_without() {
        for local in [
            "router",
            "NAS",
            "localhost",
            "*.lan",
            "*.internal",
            "*.corp",
            "*.com",
            "nas.lan",
            "x.internal",
            "x.home.arpa",
            "home.arpa",
            "*.home.arpa",
            "*.x.home.arpa",
            "a.b.Intranet",
            "x.corp",
            "x.home",
            "x.localdomain",
            "*.printers.local",
            "*.dev.localhost",
            "10.0.0.1",
            "*.0.0.1",
        ] {
            assert!(!names_the_internet(local), "{local}");
        }
        for public in [
            "registry.npmjs.org",
            "*.npmjs.org",
            "internal.example.com",
            "*.internal.example.com",
            "corp.example",
            "lan.example",
            "homearpa.example",
            "arpa.example",
        ] {
            assert!(names_the_internet(public), "{public}");
        }
    }

    fn lacked(domain: &str, tool: Option<&str>) -> NeededDomain {
        NeededDomain {
            domain: domain.into(),
            why: None,
            tool: tool.map(str::to_owned),
            frees_offline: false,
        }
    }

    #[test]
    fn a_private_registry_takes_the_failed_tools_offline_variables_or_all_of_them() {
        // Known registries: allowing them is what takes their variables away.
        for domain in [
            NPM,
            "files.pythonhosted.org",
            "static.crates.io",
            "proxy.golang.org",
            "repo.maven.apache.org",
        ] {
            for tool in [None, Some("npm")] {
                assert!(
                    runner::online_after(&[lacked(domain, tool)]).is_empty(),
                    "{domain}"
                );
            }
        }
        let private = lacked("npm.mycorp.example", None);
        assert_eq!(
            runner::online_after(std::slice::from_ref(&private)),
            EVERY_VARIABLE_IN_ORDER
        );
        // Only the tool that failed for the private domain itself narrows what goes.
        for (tool, expected) in [
            ("npm", &["npm_config_offline"][..]),
            ("pnpm", &["npm_config_offline", "pnpm_config_offline"]),
            ("cargo", &["CARGO_NET_OFFLINE"]),
        ] {
            let its_own = lacked("npm.mycorp.example", Some(tool));
            assert_eq!(runner::online_after(&[its_own]), expected, "{tool}");
            // One that failed for another domain says nothing about the private one.
            assert_eq!(
                runner::online_after(&[private.clone(), lacked(NPM, Some(tool))]),
                EVERY_VARIABLE_IN_ORDER,
                "{tool}"
            );
        }
        // A tool Keel can't tell failed offline narrows nothing.
        for tool in ["yarn", "pip"] {
            assert_eq!(
                runner::online_after(&[lacked("npm.mycorp.example", Some(tool))]),
                EVERY_VARIABLE_IN_ORDER,
                "{tool}"
            );
        }
    }

    #[test]
    fn each_domain_frees_for_the_project_only_what_it_frees_itself() {
        let lacking = [
            lacked("pip.mycorp.example", None),
            lacked(NPM, Some("npm")),
            lacked("npm.mycorp.example", Some("npm")),
        ];
        let freed = runner::freed_by(&lacking);
        let of = |domain: &str| -> Vec<&str> {
            freed
                .iter()
                .filter(|(freed_by, _)| *freed_by == domain)
                .map(|(_, variable)| *variable)
                .collect()
        };
        assert_eq!(of("pip.mycorp.example"), EVERY_VARIABLE_IN_ORDER);
        assert_eq!(of("npm.mycorp.example"), ["npm_config_offline"]);
        assert!(of(NPM).is_empty());
        assert_eq!(freed.len(), EVERY_VARIABLE_IN_ORDER.len() + 1);
    }

    #[test]
    fn a_builder_asks_only_for_single_hosts_on_the_internet() {
        let answer = json!({ "neededDomains": [
            { "domain": " Registry.NPMjs.org ", "why": " to install next " },
            { "domain": "crates.io", "why": "" },
            { "domain": "*.npmjs.org", "why": "all of it" },
            { "domain": "https://example.com", "why": "x" },
            { "domain": "localhost", "why": "x" },
            { "domain": "127.0.0.1", "why": "x" },
            { "why": "no domain" },
            { "domain": "long.example", "why": "y".repeat(1000) },
        ]});
        let found = declared(&answer);
        let names: Vec<&str> = found.iter().map(|n| n.domain.as_str()).collect();
        assert_eq!(
            names,
            [
                "registry.npmjs.org",
                "index.crates.io",
                "static.crates.io",
                "long.example"
            ]
        );
        assert_eq!(found[0].why.as_deref(), Some("to install next"));
        assert_eq!(found[1].why, None);
        assert_eq!(found[3].why.as_ref().unwrap().len(), MOST_WHY_CHARS);
        assert!(declared(&json!({ "verdict": "approved" })).is_empty());
    }

    /// A project and an engine on it, with runs of one builder written by hand: which variables a
    /// step goes without needs no sandbox to be seen.
    struct Recorded {
        dir: tempfile::TempDir,
        store: Arc<Store>,
        engine: Engine,
    }

    fn recorded_project() -> Recorded {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("keel.db")).unwrap());
        let mut config = Config::new(dir.path().join("data"));
        config.keep_awake = false;
        // No tools: what answering sets going stops before it runs anything.
        let engine = Engine::new(store.clone(), config, Tools::from_dirs(&[]), |_| {});
        store
            .add_project(&Project {
                id: "p".into(),
                name: "app".into(),
                path: dir.path().join("app").display().to_string(),
                created_at: keel_store::now(),
                github_account: None,
                github_repo: None,
                setup_command: None,
                check_command: None,
                allowed_domains: vec![],
                allowed_read: vec![],
                services: vec![],
                env: vec![],
                default_workflow: None,
                sensitive_paths: vec![],
                allow_localhost: false,
                issue_label: None,
            })
            .unwrap();
        Recorded { dir, store, engine }
    }

    impl Recorded {
        fn run(&self, run: &str) {
            let workflow = json!([
                { "type": "agent", "id": "builder", "model": "m", "effort": null, "tools": "build", "prompt": "p", "maxTurns": null, "maxBudgetUsd": null },
                { "type": "check", "id": "check", "run": "make test" },
                { "type": "approval", "id": "human" },
            ]);
            self.store
                .add_run(&NewRun {
                    id: run.into(),
                    project_id: "p".into(),
                    title: "x".into(),
                    request: "x".into(),
                    workflow: workflow.to_string(),
                    language: "en".into(),
                    source_issue: None,
                    request_written: None,
                    workflow_id: None,
                    workflow_name: None,
                    started_by: None,
                })
                .unwrap();
        }

        fn builder(&self, run: &str, round: i64, number: i64, state: AttemptState) -> Attempt {
            self.attempt(run, "builder", round, number, state, "{}")
        }

        /// A try of the run's check, with the settings it was set going with.
        fn check(&self, run: &str, number: i64, state: AttemptState, settings: &str) -> Attempt {
            self.attempt(run, "check", 0, number, state, settings)
        }

        fn attempt(
            &self,
            run: &str,
            step: &str,
            round: i64,
            number: i64,
            state: AttemptState,
            settings: &str,
        ) -> Attempt {
            let id = format!("{run}-{step}-{number}");
            let builds = step == "builder";
            let files = self.dir.path().join("files");
            std::fs::create_dir_all(&files).unwrap();
            self.store
                .add_attempt(&NewAttempt {
                    id: id.clone(),
                    run_id: run.into(),
                    step_id: step.into(),
                    kind: if builds { "agent" } else { "check" }.into(),
                    role: "worker".into(),
                    round,
                    number,
                    state,
                    model_requested: if builds { "m".into() } else { String::new() },
                    effort: None,
                    tools: if builds { "build" } else { "check" }.into(),
                    max_turns: None,
                    max_budget_usd: None,
                    language: "en".into(),
                    prompt: if builds { "p" } else { "make test" }.into(),
                    system_prompt_sources: "[]".into(),
                    settings: settings.into(),
                    session_id: id.clone(),
                    resumed_from: None,
                    workspace: self.dir.path().join("nowhere").display().to_string(),
                    raw_path: files.join(format!("{id}.log")).display().to_string(),
                    started_at: keel_store::now(),
                })
                .unwrap();
            self.store.attempt(&id).unwrap()
        }

        fn kinds(&self, attempt: &Attempt) -> Vec<String> {
            let events = self.store.events(&attempt.id, 0).unwrap();
            events.into_iter().map(|event| event.kind).collect()
        }

        fn state(&self, attempt: &Attempt) -> String {
            self.store.attempt(&attempt.id).unwrap().state
        }

        /// A builder's try that waits for these, as it ended.
        fn waiting(&self, run: &str, number: i64, domains: &[&str]) -> Attempt {
            let attempt = self.builder(run, 0, number, AttemptState::Blocked);
            let reason = Reason::NetworkNeeded {
                needed: domains.iter().map(|domain| lacked(domain, None)).collect(),
            };
            self.store
                .set_state(&attempt.id, AttemptState::Blocked, Some(&reason))
                .unwrap();
            attempt
        }

        fn online(&self, attempt: &Attempt) -> Vec<String> {
            let mut online = self.engine.inner.online_for_builder(attempt).unwrap();
            online.sort();
            online.dedup();
            online
        }
    }

    /// As `runner::OFFLINE` lists them.
    const EVERY_VARIABLE_IN_ORDER: [&str; 7] = [
        "CARGO_NET_OFFLINE",
        "GOPROXY",
        "npm_config_offline",
        "pnpm_config_offline",
        "PIP_NO_INDEX",
        "UV_OFFLINE",
        "MAVEN_ARGS",
    ];

    const EVERY_VARIABLE: [&str; 7] = [
        "CARGO_NET_OFFLINE",
        "GOPROXY",
        "MAVEN_ARGS",
        "PIP_NO_INDEX",
        "UV_OFFLINE",
        "npm_config_offline",
        "pnpm_config_offline",
    ];

    #[test]
    fn what_every_wait_of_a_step_allowed_holds_for_its_later_tries_in_that_round() {
        let w = recorded_project();
        w.run("a");
        // A private registry first, and then one Keel knows, each allowed.
        for (number, domain) in [(1, "npm.mycorp.example"), (2, "pypi.org")] {
            let waited = w.waiting("a", number, &[domain]);
            w.store
                .append_event(&waited.id, "networkAllowed", &json!({}))
                .unwrap();
        }
        w.builder("a", 0, 3, AttemptState::Stopped);
        // Tried again without going on with any session.
        let again = w.builder("a", 0, 4, AttemptState::Queued);

        let mut online = w.engine.inner.online_for_step(&again).unwrap();
        online.sort();
        assert_eq!(online, EVERY_VARIABLE);
        // One of another round isn't a try of those.
        let later = w.builder("a", 1, 5, AttemptState::Queued);
        assert!(w.engine.inner.online_for_step(&later).unwrap().is_empty());
    }

    #[test]
    fn a_private_domain_allowed_keeps_the_builder_online_in_later_rounds_and_runs() {
        let w = recorded_project();
        w.run("a");
        let waited = w.waiting("a", 1, &["npm.mycorp.example"]);

        w.engine
            .answer_network(&waited.id, NetworkAnswer::Allow)
            .unwrap();

        let later = w.builder("a", 1, 10, AttemptState::Queued);
        assert_eq!(w.online(&later), EVERY_VARIABLE);
        w.run("b");
        let next = w.builder("b", 0, 1, AttemptState::Queued);
        assert_eq!(w.online(&next), EVERY_VARIABLE);
        // Not once the project doesn't allow it.
        w.engine
            .update_project("p", &ProjectSettings::default())
            .unwrap();
        assert!(w.online(&next).is_empty());
    }

    #[test]
    fn a_known_registry_allowed_frees_nothing_beyond_its_domain() {
        let w = recorded_project();
        w.run("a");
        let waited = w.waiting("a", 1, &[NPM]);

        w.engine
            .answer_network(&waited.id, NetworkAnswer::Allow)
            .unwrap();

        // What takes its variables away is that the project allows it: see `runner::OFFLINE`.
        w.run("b");
        let next = w.builder("b", 0, 1, AttemptState::Queued);
        assert!(w.online(&next).is_empty());
    }

    const NPM_OFFLINE: &str = "npm error code ENOTCACHED\nnpm error request to https://registry.npmjs.org/next failed: cache mode is 'only-if-cached' but no cached response is available.";

    /// A builder declares a private registry of pip's, and its last command is npm failing for
    /// being offline: nothing says the private one is npm's.
    #[test]
    fn a_private_domain_declared_beside_a_tool_that_failed_offline_frees_every_variable() {
        let w = recorded_project();
        w.run("a");
        let built = w.builder("a", 0, 1, AttemptState::Running);
        for (kind, data) in [
            ("toolCall", json!({ "id": "t1", "name": "Bash" })),
            (
                "toolResult",
                json!({ "id": "t1", "isError": true, "text": NPM_OFFLINE }),
            ),
        ] {
            w.store.append_event(&built.id, kind, &data).unwrap();
        }
        let answer = json!({ "message": "x", "neededDomains": [
            { "domain": "pip.mycorp.example", "why": "our own packages" },
        ]});
        let (mut state, mut reason) = (AttemptState::Succeeded, None);

        w.engine
            .inner
            .waits_for_network(&built.id, &mut state, &mut reason, Some(&answer));

        assert_eq!(state, AttemptState::Blocked);
        let Some(Reason::NetworkNeeded { needed }) = reason else {
            panic!("not waiting: {reason:?}");
        };
        let said: Vec<_> = needed
            .iter()
            .map(|n| (n.domain.as_str(), n.tool.as_deref(), n.frees_offline))
            .collect();
        assert_eq!(
            said,
            [
                ("pip.mycorp.example", None, true),
                (NPM, Some("npm"), false)
            ]
        );
        assert!(runner::online_after(&needed).contains(&"PIP_NO_INDEX"));
        // It's on the record as it waits, and allowing it frees pip's in later runs too.
        assert_eq!(w.state(&built), "blocked");
        w.engine
            .answer_network(&built.id, NetworkAnswer::Allow)
            .unwrap();
        w.run("b");
        let next = w.builder("b", 0, 1, AttemptState::Queued);
        assert_eq!(w.online(&next), EVERY_VARIABLE);
    }

    /// A check that is closing after it failed as npm does offline, as `finish` hands it over.
    fn closing_offline(w: &Recorded, run: &str) -> (Attempt, AttemptState, Option<Reason>) {
        closing_with(w, run, NPM_OFFLINE)
    }

    /// A check that is closing after it failed, having printed this.
    fn closing_with(
        w: &Recorded,
        run: &str,
        printed: &str,
    ) -> (Attempt, AttemptState, Option<Reason>) {
        w.run(run);
        w.builder(run, 0, 1, AttemptState::Succeeded);
        let check = w.check(run, 2, AttemptState::Running, "{}");
        w.store
            .append_event(
                &check.id,
                "checkResult",
                &json!({ "stdout": "", "stderr": printed }),
            )
            .unwrap();
        let failed = Reason::CheckFailed {
            exit_code: Some(1),
            localhost_refused: false,
            failed_times: None,
            unchanged: false,
        };
        (check, AttemptState::Failed, Some(failed))
    }

    #[test]
    fn a_stop_asked_for_while_a_step_that_lacks_a_domain_closes_wins_over_waiting_for_it() {
        let w = recorded_project();
        let (check, mut state, mut reason) = closing_offline(&w, "a");
        // The check reads as running until it's closed, so the stop is taken.
        w.engine.stop(&check.id).unwrap();

        w.engine
            .inner
            .waits_for_network(&check.id, &mut state, &mut reason, None);

        assert_eq!((state, reason), (AttemptState::Stopped, None));
        assert_eq!(w.state(&check), "running", "nothing says it waits");
        // Stopped is no failed check either, so the work doesn't go back.
        let mut reason = None;
        assert!(
            !w.engine
                .inner
                .failed_check_goes_back(&check.id, &mut state, &mut reason)
        );
        assert_eq!(w.store.run("a").unwrap().round, 0);
    }

    #[test]
    fn a_stop_that_comes_once_a_step_waits_for_a_domain_is_refused_instead_of_lost() {
        let w = recorded_project();
        let (check, mut state, mut reason) = closing_offline(&w, "a");

        w.engine
            .inner
            .waits_for_network(&check.id, &mut state, &mut reason, None);

        assert_eq!(state, AttemptState::Blocked);
        assert!(matches!(reason, Some(Reason::NetworkNeeded { .. })));
        assert_eq!(w.state(&check), "blocked");
        assert!(matches!(w.engine.stop(&check.id), Err(Error::InvalidState)));
        assert!(!w.kinds(&check).contains(&"stopRequested".to_owned()));
        // Stopping it is then an answer to what it waits for.
        w.engine
            .answer_network(&check.id, NetworkAnswer::Stop)
            .unwrap();
        assert_eq!(w.state(&check), "stopped");
    }

    #[test]
    fn allowing_a_domain_to_a_step_that_cannot_start_says_it_was_added_and_leaves_it_waiting() {
        let w = recorded_project();
        w.run("a");
        let waited = w.waiting("a", 1, &["npm.mycorp.example"]);
        // Where the run keeps its files there's a file, so no step of it can start.
        let runs = w.dir.path().join("data/runs");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::write(runs.join("a"), "").unwrap();

        let refused = w.engine.answer_network(&waited.id, NetworkAnswer::Allow);

        let Err(Error::Blocked(Reason::DomainAllowedNotStarted { domains, detail })) = refused
        else {
            panic!("it started, or didn't say: {refused:?}");
        };
        assert_eq!(domains, ["npm.mycorp.example"]);
        assert!(!detail.is_empty());
        // The project has it, and nothing says the step was repeated with it.
        let allowed = w.store.project("p").unwrap().allowed_domains;
        assert_eq!(allowed, ["npm.mycorp.example"]);
        assert!(!w.kinds(&waited).contains(&"networkAllowed".to_owned()));
        assert_eq!(w.store.attempts_of_run("a").unwrap().len(), 1);
        assert!(w.engine.inner.allowed_wait(&waited.id).unwrap().is_none());

        // It still waits: once the step can start, allowing it again repeats it.
        std::fs::remove_file(runs.join("a")).unwrap();
        w.engine
            .answer_network(&waited.id, NetworkAnswer::Allow)
            .unwrap();
        assert!(w.kinds(&waited).contains(&"networkAllowed".to_owned()));
        assert_eq!(w.store.attempts_of_run("a").unwrap().len(), 2);
        assert_eq!(allowed, w.store.project("p").unwrap().allowed_domains);
    }

    /// A check repeated with the network that the app closed under, as it's found on reopening.
    fn found_on_reopening(w: &Recorded, run: &str, exit_code: Option<&str>) -> Attempt {
        w.run(run);
        w.builder(run, 0, 1, AttemptState::Succeeded);
        // The try that waited for npm's registry, which a person allowed.
        let waited = w.check(run, 1, AttemptState::Blocked, "{}");
        let reason = Reason::NetworkNeeded {
            needed: vec![lacked(NPM, Some("npm"))],
        };
        w.store
            .set_state(&waited.id, AttemptState::Blocked, Some(&reason))
            .unwrap();
        w.store
            .append_event(&waited.id, "networkAllowed", &json!({ "domains": [NPM] }))
            .unwrap();
        let settings = json!({ "network": "allowed domains", "allowedDomains": [NPM] });
        let check = w.check(run, 2, AttemptState::Running, &settings.to_string());
        std::fs::write(&check.raw_path, "").unwrap();
        if let Some(code) = exit_code {
            let left = keel_harness::process::exit_code_file(Path::new(&check.raw_path));
            std::fs::write(left, code).unwrap();
        }
        w.engine.inner.recover_attempt(&check).unwrap();
        w.store.attempt(&check.id).unwrap()
    }

    /// Its proxy went with the app while it ran, so a command that then failed may have failed
    /// for that: npm exits with 1 when its connection is refused.
    #[test]
    fn a_check_with_the_network_that_failed_while_the_app_was_away_did_not_fail_on_the_work() {
        let w = recorded_project();

        let failed = found_on_reopening(&w, "a", Some("1"));

        assert!(w.kinds(&failed).contains(&"networkLost".to_owned()));
        // It stays with the person, and no round goes by for it.
        assert_eq!(failed.state, "failed");
        assert_eq!(w.store.run("a").unwrap().round, 0);
        assert_eq!(w.store.attempts_of_run("a").unwrap().len(), 3);
        // Tried again, it has the network it lost.
        w.store
            .update_project(
                "p",
                &ProjectSettings {
                    allowed_domains: vec![NPM.into()],
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
        let again = w.engine.retry(&failed.id, false).unwrap();
        let again = w.store.attempt(&again).unwrap();
        assert_eq!(again.settings["allowedDomains"], json!([NPM]));
    }

    #[test]
    fn a_check_with_the_network_that_passed_while_the_app_was_away_passed() {
        let w = recorded_project();
        let passed = found_on_reopening(&w, "b", Some("0"));
        assert!(!w.kinds(&passed).contains(&"networkLost".to_owned()));
        assert_eq!(passed.state, "succeeded");
    }

    #[test]
    fn a_check_with_the_network_that_left_no_exit_code_lost_it_with_the_app() {
        let w = recorded_project();
        // Gone with nothing said, or ended by a signal, which is no code to go by.
        for (run, left) in [("a", None), ("b", Some("143"))] {
            let lost = found_on_reopening(&w, run, left);
            assert!(w.kinds(&lost).contains(&"networkLost".to_owned()), "{run}");
            assert_eq!(lost.state, "interrupted", "{run}");
            assert_eq!(w.store.run(run).unwrap().round, 0, "{run}");
        }
    }

    /// One without the network lost nothing: how it ended while the app was away is how it ended.
    #[test]
    fn a_check_without_the_network_that_failed_while_the_app_was_away_failed_on_the_work() {
        let w = recorded_project();
        w.run("a");
        w.builder("a", 0, 1, AttemptState::Succeeded);
        let plain = w.check("a", 2, AttemptState::Running, "{}");
        std::fs::write(&plain.raw_path, "").unwrap();
        let left = keel_harness::process::exit_code_file(Path::new(&plain.raw_path));
        std::fs::write(left, "1").unwrap();

        w.engine.inner.recover_attempt(&plain).unwrap();

        assert!(!w.kinds(&plain).contains(&"networkLost".to_owned()));
        assert_eq!(w.state(&plain), "returned");
        assert_eq!(w.store.run("a").unwrap().round, 1);
    }

    /// A step is on the record as waiting a moment before it's done closing, and from then on a
    /// person can answer it.
    #[test]
    fn an_answer_that_comes_while_a_step_that_waits_is_still_closing_is_not_undone() {
        let w = recorded_project();
        w.run("a");
        w.builder("a", 0, 1, AttemptState::Succeeded);
        let check = w.check("a", 2, AttemptState::Running, "{}");
        // It ended while the app was away, failing as npm does offline: reopening closes it.
        let raw = Path::new(&check.raw_path);
        std::fs::write(raw, "").unwrap();
        std::fs::write(raw.with_file_name("a-check-2.stderr.txt"), NPM_OFFLINE).unwrap();
        std::fs::write(keel_harness::process::exit_code_file(raw), "1").unwrap();
        let engine = w.engine.clone();
        let answered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let said = answered.clone();
        *locked(&w.engine.inner.once_it_waits) = Some(Box::new(move |attempt| {
            let stopped = engine.stop_waiting_by(attempt, "bot");
            said.lock().unwrap().push(stopped.is_ok());
        }));

        w.engine.inner.recover_attempt(&check).unwrap();

        assert_eq!(
            *answered.lock().unwrap(),
            [true],
            "it waited, and was told to stop"
        );
        assert_eq!(w.state(&check), "stopped");
        assert!(w.store.attempt(&check.id).unwrap().reason.is_none());
    }

    #[test]
    fn going_on_without_a_domain_is_on_the_record_only_once_the_step_went_on() {
        let w = recorded_project();
        w.run("a");
        let waited = w.waiting("a", 1, &[NPM]);
        // What a later step of the run that lacks the same domain is asked about.
        let later = w.check("a", 2, AttemptState::Running, "{}");
        w.store
            .append_event(
                &later.id,
                "checkResult",
                &json!({ "stdout": "", "stderr": NPM_OFFLINE }),
            )
            .unwrap();
        let failed = Reason::CheckFailed {
            exit_code: Some(1),
            localhost_refused: false,
            failed_times: None,
            unchanged: false,
        };
        let asks = || {
            let found =
                w.engine
                    .inner
                    .lacks_network(&later.id, AttemptState::Failed, Some(&failed), None);
            found.map(|needed| needed.len())
        };
        // The store refuses to move the step on.
        let refusing = rusqlite::Connection::open(w.dir.path().join("keel.db")).unwrap();
        refusing
            .execute_batch(
                "create trigger no_going_on before update of state on attempt
                 begin select raise(abort, 'not now'); end;",
            )
            .unwrap();

        let refused = w
            .engine
            .answer_network(&waited.id, NetworkAnswer::GoWithout);

        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(w.state(&waited), "blocked");
        assert!(!w.kinds(&waited).contains(&"networkDeclined".to_owned()));
        // So a later step that lacks it still asks.
        assert_eq!(asks(), Some(1));

        // Once it can go on, the answer is on the record, and nothing asks again.
        refusing.execute_batch("drop trigger no_going_on;").unwrap();
        w.engine
            .answer_network(&waited.id, NetworkAnswer::GoWithout)
            .unwrap();
        assert_eq!(w.state(&waited), "succeeded");
        let kinds = w.kinds(&waited);
        let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
        assert!(at("networkDeclined") < at("finished"), "{kinds:?}");
        assert_eq!(asks(), None);
    }

    fn tools(output: &str) -> Vec<&'static str> {
        failed_offline(output).iter().map(|o| o.tool).collect()
    }

    #[test]
    fn the_errors_of_an_offline_tool_are_told_by_their_own_lines() {
        for (output, tool) in [
            (
                "npm error code ENOTCACHED\nnpm error request to https://registry.npmjs.org/next failed: cache mode is 'only-if-cached' but no cached response is available.",
                "npm",
            ),
            ("npm ERR! code ENOTCACHED\n", "npm"),
            (
                " ERR_PNPM_NO_OFFLINE_TARBALL  A package is missing from the store but cannot download it in offline mode. The missing package may be downloaded from https://registry.npmjs.org/next/-/next-15.3.9.tgz.",
                "pnpm",
            ),
            (
                "[ERR_PNPM_NO_OFFLINE_META] Failed to resolve next@15.3.9 in package mirror",
                "pnpm",
            ),
            (
                "error: failed to download `serde v1.0.0`\n\nCaused by:\n  attempting to make an HTTP request, but --offline was specified\n",
                "cargo",
            ),
        ] {
            assert_eq!(tools(output), [tool], "{output}");
        }
    }

    /// Kept from its index, pip says of a package PyPI has what it says of one that's misspelt, or
    /// that only a private index or a folder of wheels has.
    const PIP_FOUND_NONE: &str = "Looking in links: ./wheels\nERROR: Could not find a version that satisfies the requirement acme-internal==2 (from versions: none)\nERROR: No matching distribution found for acme-internal==2\n";

    #[test]
    fn pip_finding_no_version_does_not_say_it_lacked_pypi() {
        assert!(tools(PIP_FOUND_NONE).is_empty());

        // A check that fails that way is a check that failed: nothing offers to open PyPI.
        let w = recorded_project();
        let (check, mut state, mut reason) = closing_with(&w, "a", PIP_FOUND_NONE);
        let waits = w
            .engine
            .inner
            .waits_for_network(&check.id, &mut state, &mut reason, None);
        assert!(!waits);
        assert_eq!(state, AttemptState::Failed);
        assert!(matches!(reason, Some(Reason::CheckFailed { .. })));
        assert_eq!(w.state(&check), "running", "nothing says it waits");
        // And so is a builder whose last command did: what it lacked is its own to say.
        let built = w.builder("a", 0, 3, AttemptState::Running);
        for (kind, data) in [
            ("toolCall", json!({ "id": "t1", "name": "Bash" })),
            (
                "toolResult",
                json!({ "id": "t1", "isError": true, "text": PIP_FOUND_NONE }),
            ),
        ] {
            w.store.append_event(&built.id, kind, &data).unwrap();
        }
        let answer = json!({ "message": "x", "neededDomains": [] });
        let (mut state, mut reason) = (AttemptState::Succeeded, None);
        assert!(!w.engine.inner.waits_for_network(
            &built.id,
            &mut state,
            &mut reason,
            Some(&answer)
        ));
        assert_eq!((state, reason), (AttemptState::Succeeded, None));
    }

    #[test]
    fn anything_that_could_mean_something_else_is_not_an_offline_error() {
        for output in [
            "test prints \"npm error code ENOTCACHED\" and passes",
            "ENOTCACHED",
            "npm error code E404",
            // Another registry, which allowing npm's wouldn't reach.
            "npm error code ENOTCACHED\nnpm error request to https://npm.example.com/x failed",
            "npm error code ENOTCACHED\nnpm error request to https://registry.npmjs.org.evil.example/x failed",
            " ERR_PNPM_NO_OFFLINE_TARBALL  The missing package may be downloaded from https://npm.example.com/x.tgz.",
            "error: no matching package named `serd` found\nAs a reminder, you're using offline mode (--offline)",
            "error: failed to download `x v1.0.0 (registry `mine`)`\n  attempting to make an HTTP request, but --offline was specified",
            "ERROR: Could not find a version that satisfies the requirement requests==99 (from versions: 1.0, 2.0)\nERROR: No matching distribution found for requests==99",
            "ERROR: No matching distribution found for requests",
        ] {
            assert!(tools(output).is_empty(), "{output}");
        }
    }

    #[test]
    fn only_the_last_command_a_builder_ran_says_it_could_not_finish() {
        let event = |kind: &str, data: Value| Event {
            seq: 0,
            attempt_id: String::new(),
            at: String::new(),
            kind: kind.into(),
            data,
        };
        let call = |id: &str, name: &str| event("toolCall", json!({ "id": id, "name": name }));
        let result = |id: &str, failed: bool, text: &str| {
            event(
                "toolResult",
                json!({ "id": id, "isError": failed, "text": text }),
            )
        };
        let failed = [call("1", "Bash"), result("1", true, "offline")];
        assert_eq!(last_command_failure(&failed), Some("offline"));
        // Reading a file afterwards isn't a command.
        let read = [
            call("1", "Bash"),
            result("1", true, "offline"),
            call("2", "Read"),
            result("2", false, "text"),
        ];
        assert_eq!(last_command_failure(&read), Some("offline"));
        let got_past = [
            call("1", "Bash"),
            result("1", true, "offline"),
            call("2", "Bash"),
            result("2", false, "ok"),
        ];
        assert_eq!(last_command_failure(&got_past), None);
        assert_eq!(last_command_failure(&[]), None);
    }
}
