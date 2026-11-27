//! Adding and configuring projects for an MCP client the person lets do it. Only a project's GitHub
//! account and its setup and check commands are a client's to set: what its commands may reach
//! stays with the person. Each write is recorded with what it set, in one go with it.

use std::path::PathBuf;

use keel_store::{ClientProjectEdit, ClientWrite, Project};

use crate::project_proposals::{hides_text, readable_path};
use crate::{AddOutcome, AddedProject, Engine, Error, Result};

/// Longer than any command a person would read through, which they have to be able to.
const MAX_COMMAND_CHARS: usize = 4096;

/// Where a project a client adds comes from.
#[derive(Debug, Clone)]
pub enum ProjectSource {
    /// A folder on this Mac, or one inside it.
    Folder(PathBuf),
    /// A proposal waiting for the person, by its id.
    Proposal(String),
}

/// A project's GitHub account and commands as a client gives them. When configuring, `None` leaves a
/// value as it is. An empty command means the project has none, as in the app.
#[derive(Debug, Clone, Default)]
pub struct ProjectValues {
    pub github_account: Option<String>,
    pub setup_command: Option<String>,
    pub check_command: Option<String>,
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
    /// The project was added or removed while Keel was looking.
    ProjectChanged,
}

/// A command trimmed, as the app saves it. Empty means none.
fn command(field: &'static str, text: &str) -> Result<Option<String>, ClientRefusal> {
    let text = text.trim();
    if text.chars().count() > MAX_COMMAND_CHARS || text.chars().any(hides_text) {
        return Err(ClientRefusal::UnreadableCommand { field });
    }
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// What to write for these values, or why one of them can't be.
fn edit_of(values: &ProjectValues) -> Result<ClientProjectEdit, ClientRefusal> {
    let read =
        |field, text: &Option<String>| text.as_deref().map(|text| command(field, text)).transpose();
    Ok(ClientProjectEdit {
        github_account: values
            .github_account
            .as_deref()
            .map(|login| login.trim().to_owned()),
        setup_command: read("setupCommand", &values.setup_command)?,
        check_command: read("checkCommand", &values.check_command)?,
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
        if !readable_path(&root_text) {
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
        if let Some(refusal) = self.unknown_account(&login)? {
            return Ok(Err(refusal));
        }

        let (id, outcome) = match &existing {
            Some(removed) => {
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
                let project = self.refresh_repo(&id)?;
                if existing.and_then(|before| before.github_account) != project.github_account {
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

    /// Changes a project's GitHub account and commands for a client, whether or not it has runs
    /// open, as the app does.
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
            && let Some(refusal) = self.unknown_account(login)?
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

    fn unknown_account(&self, login: &str) -> Result<Option<ClientRefusal>> {
        let known: Vec<String> = self
            .github_accounts()?
            .into_iter()
            .map(|account| account.login)
            .collect();
        Ok(
            (!known.iter().any(|known| known == login)).then(|| ClientRefusal::UnknownAccount {
                login: login.to_owned(),
                known,
            }),
        )
    }
}
