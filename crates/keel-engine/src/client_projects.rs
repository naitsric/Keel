//! Adding and configuring projects for an MCP client the person lets do it. Only a project's GitHub
//! account, its setup and check commands and its allowed domains are a client's to set: the rest of
//! what its commands may reach stays with the person. Each write is recorded with what it set, in
//! one go with it.

use std::path::PathBuf;

use keel_core::Reason;
use keel_store::{ClientProjectEdit, ClientWrite, Project};

use crate::network::names_the_internet;
use crate::project_proposals::{hides_text, readable_path_for_client};
use crate::{AddOutcome, AddedProject, Engine, Error, Result, is_domain};

/// Longer than any command a person would read through, which they have to be able to.
const MAX_COMMAND_CHARS: usize = 4096;

/// The most domains a client adds in one call, so that the person can read each one where Keel
/// shows what changed, and none hides among hundreds.
pub const MOST_DOMAINS_ADDED: usize = 20;

/// Where a project a client adds comes from.
#[derive(Debug, Clone)]
pub enum ProjectSource {
    /// A folder on this Mac, or one inside it.
    Folder(PathBuf),
    /// A proposal waiting for the person, by its id.
    Proposal(String),
}

/// A project's GitHub account, commands and domains as a client gives them. When configuring,
/// `None` leaves a value as it is. An empty command means the project has none, as in the app.
/// Domains only configuring changes.
#[derive(Debug, Clone, Default)]
pub struct ProjectValues {
    pub github_account: Option<String>,
    pub setup_command: Option<String>,
    pub check_command: Option<String>,
    pub add_domains: Vec<String>,
    pub remove_domains: Vec<String>,
}

/// Why Keel didn't do what a client asked, with nothing added or changed.
#[derive(Debug, Clone)]
pub enum ClientRefusal {
    /// The client may not add or configure projects.
    NotAllowed,
    UnknownProposal,
    /// The folder's path has characters that could hide what it is.
    UnreadablePath,
    AlreadyInKeel(Box<Project>),
    /// `gh` couldn't say which accounts it has, so the one given couldn't be checked.
    AccountsUnavailable {
        detail: String,
    },
    /// `gh` has no account by that name. `known` are the ones it has.
    UnknownAccount {
        login: String,
        known: Vec<String>,
    },
    /// A command the person couldn't read for what it is: too long, on several lines, or with
    /// characters that hide text.
    UnreadableCommand {
        field: &'static str,
    },
    /// Not a host name with an optional `*.`: see `is_domain`.
    InvalidDomain {
        domain: String,
    },
    /// This Mac, the local network or an IP address, which only the person allows.
    LocalDomain {
        domain: String,
    },
    /// More domains to add at once than `MOST_DOMAINS_ADDED`.
    TooManyDomains {
        given: usize,
    },
    /// The same domain to add and to remove, which doesn't say what's wanted.
    DomainBothWays {
        domain: String,
    },
    /// The project was added or removed while Keel was looking.
    ProjectChanged,
}

/// A blank that isn't the plain space: it looks like one, and the shell doesn't split words on it,
/// so `make test\u{2003}#; curl … | sh` would read as a comment and run.
fn looks_like_a_space(c: char) -> bool {
    c != ' ' && (c.is_whitespace() || c == '\u{2800}')
}

/// A command trimmed, as the app saves it. Empty means none.
fn command(field: &'static str, text: &str) -> Result<Option<String>, ClientRefusal> {
    let text = text.trim();
    if text.chars().count() > MAX_COMMAND_CHARS
        || text.chars().any(|c| hides_text(c) || looks_like_a_space(c))
    {
        return Err(ClientRefusal::UnreadableCommand { field });
    }
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// What to write for these values, or why one of them can't be.
fn edit_of(values: &ProjectValues) -> Result<ClientProjectEdit, ClientRefusal> {
    let read =
        |field, text: &Option<String>| text.as_deref().map(|text| command(field, text)).transpose();
    // Only what's added is checked. Taking one away opens nothing, so anything saved can go, one
    // from before domains were checked included, and any number of them.
    if values.add_domains.len() > MOST_DOMAINS_ADDED {
        return Err(ClientRefusal::TooManyDomains {
            given: values.add_domains.len(),
        });
    }
    if let Some(wrong) = values.add_domains.iter().find(|domain| !is_domain(domain)) {
        return Err(ClientRefusal::InvalidDomain {
            domain: wrong.clone(),
        });
    }
    if let Some(local) = values
        .add_domains
        .iter()
        .find(|domain| !names_the_internet(domain))
    {
        return Err(ClientRefusal::LocalDomain {
            domain: local.clone(),
        });
    }
    if let Some(both) = values.add_domains.iter().find(|added| {
        values
            .remove_domains
            .iter()
            .any(|removed| removed.eq_ignore_ascii_case(added))
    }) {
        return Err(ClientRefusal::DomainBothWays {
            domain: both.clone(),
        });
    }
    Ok(ClientProjectEdit {
        github_account: values
            .github_account
            .as_deref()
            .map(|login| login.trim().to_owned()),
        setup_command: read("setupCommand", &values.setup_command)?,
        check_command: read("checkCommand", &values.check_command)?,
        add_domains: values.add_domains.clone(),
        remove_domains: values.remove_domains.clone(),
        github_repo: None,
    })
}

fn refusal_of(write: ClientWrite) -> Option<ClientRefusal> {
    match write {
        ClientWrite::Done => None,
        ClientWrite::NotAllowed => Some(ClientRefusal::NotAllowed),
        ClientWrite::ProjectChanged => Some(ClientRefusal::ProjectChanged),
    }
}

impl Engine {
    /// Adds a folder as when the person adds one, map included, with the account and commands the
    /// client gives. A command it leaves out means none, also on a removed project, which comes
    /// back with its history. One already in use is refused. With the project comes the run of its
    /// map, if one started, which is the client's as a run it started.
    pub fn add_project_for_client(
        &self,
        client_id: &str,
        source: &ProjectSource,
        values: &ProjectValues,
        language: &str,
    ) -> Result<Result<(AddedProject, Option<String>), ClientRefusal>> {
        let mut edit = match edit_of(values) {
            Ok(edit) => edit,
            Err(refusal) => return Ok(Err(refusal)),
        };
        edit.setup_command.get_or_insert(None);
        edit.check_command.get_or_insert(None);
        let git = self.git()?;
        let not_a_repo = |path: &PathBuf| Error::NotARepo(path.to_string_lossy().into_owned());
        let root = match source {
            ProjectSource::Folder(path) => git.toplevel(path).map_err(|_| not_a_repo(path))?,
            ProjectSource::Proposal(id) => {
                let Some(proposal) = self
                    .project_proposals()?
                    .into_iter()
                    .find(|proposal| proposal.id == *id)
                else {
                    return Ok(Err(ClientRefusal::UnknownProposal));
                };
                // That folder, and not whatever's around it by now.
                let root = PathBuf::from(proposal.path);
                if git.toplevel(&root).ok().as_ref() != Some(&root) {
                    return Err(not_a_repo(&root));
                }
                root
            }
        };
        let root_text = root.to_string_lossy().into_owned();
        if !readable_path_for_client(&root_text) {
            return Ok(Err(ClientRefusal::UnreadablePath));
        }
        let store = &self.inner.store;
        let existing = store.project_by_path(&root_text)?;
        if let Some(project) = &existing
            && !store.project_is_removed(&project.id)?
        {
            return Ok(Err(ClientRefusal::AlreadyInKeel(Box::new(project.clone()))));
        }
        let login = edit.github_account.clone().unwrap_or_default();
        if let Some(refusal) = self.account_refusal(&login)? {
            return Ok(Err(refusal));
        }

        let (id, outcome) = match &existing {
            Some(removed) => {
                // As `origin` says now, in the same write: it may have changed while removed.
                edit.github_repo = Some(
                    git.origin_url(&root)
                        .and_then(|url| keel_harness::github::github_repo(&url)),
                );
                let write =
                    store.edit_project_for_mcp_client(client_id, &removed.id, &edit, true)?;
                if let Some(refusal) = refusal_of(write) {
                    return Ok(Err(refusal));
                }
                (removed.id.clone(), AddOutcome::Restored)
            }
            None => {
                let mut project = crate::new_project(&git, &root);
                project.github_account = edit.github_account.clone();
                project.setup_command = edit.setup_command.clone().flatten();
                project.check_command = edit.check_command.clone().flatten();
                let write = store.add_project_for_mcp_client(client_id, &project)?;
                if let Some(refusal) = refusal_of(write) {
                    return Ok(Err(refusal));
                }
                (project.id, AddOutcome::New)
            }
        };
        self.inner.settle_project_proposals(&root_text);
        let (project, (map, map_run)) = match outcome {
            AddOutcome::New => {
                let map = self.map_new_project(&id, language);
                (store.project(&id)?, map)
            }
            _ => {
                let project = store.project(&id)?;
                let moved = existing.is_some_and(|before| {
                    before.github_account != project.github_account
                        || before.github_repo != project.github_repo
                });
                if moved {
                    self.inner.compare_map_with_github(&project)?;
                }
                (project, (None, None))
            }
        };
        // For the app to show it, and for the board to look at the cards a restored one had in Todo.
        self.inner.changed("");
        Ok(Ok((
            AddedProject {
                project,
                outcome,
                map,
            },
            map_run,
        )))
    }

    /// Changes a project's GitHub account, commands and domains for a client, whether or not it
    /// has runs open, as the app does.
    pub fn configure_project_for_client(
        &self,
        client_id: &str,
        project_id: &str,
        values: &ProjectValues,
    ) -> Result<Result<Project, ClientRefusal>> {
        let edit = match edit_of(values) {
            Ok(edit) => edit,
            Err(refusal) => return Ok(Err(refusal)),
        };
        if let Some(login) = &edit.github_account
            && let Some(refusal) = self.account_refusal(login)?
        {
            return Ok(Err(refusal));
        }
        let store = &self.inner.store;
        let before = store.project(project_id)?.github_account;
        let write = store.edit_project_for_mcp_client(client_id, project_id, &edit, false)?;
        if let Some(refusal) = refusal_of(write) {
            return Ok(Err(refusal));
        }
        let project = store.project(project_id)?;
        if project.github_account != before {
            self.inner.compare_map_with_github(&project)?;
        }
        self.inner.changed("");
        Ok(Ok(project))
    }

    /// Why this account can't be a project's, if it can't.
    fn account_refusal(&self, login: &str) -> Result<Option<ClientRefusal>> {
        let accounts = match self.github_accounts() {
            Ok(accounts) => accounts,
            Err(Error::Blocked(Reason::DeliveryFailed { detail, .. })) => {
                return Ok(Some(ClientRefusal::AccountsUnavailable { detail }));
            }
            Err(error) => return Err(error),
        };
        let known: Vec<String> = accounts.into_iter().map(|account| account.login).collect();
        Ok(
            (!known.iter().any(|known| known == login)).then(|| ClientRefusal::UnknownAccount {
                login: login.to_owned(),
                known,
            }),
        )
    }
}
