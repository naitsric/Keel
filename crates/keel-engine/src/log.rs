//! Where an error goes when the engine carries on without it: Keel's log, in its data folder.
//!
//! A step's monitor has nobody to return an error to, and a run that can't write down how a step
//! ended, or can't start its next one, would otherwise just stop with nothing to say why. None of
//! these change what the caller does next. They only make sure the failure is written somewhere.

use std::fmt::Display;

use keel_core::{AttemptState, Log, Reason};
use keel_store::rusqlite_value::Value;

use crate::Inner;

impl Inner {
    pub(crate) fn log(&self) -> Log {
        Log::in_dir(&self.config.data_dir)
    }

    /// A result the caller goes on without, for the step or run `id`. A failure is written to
    /// the log with what was being done.
    pub(crate) fn noted<T, E: Display>(
        &self,
        what: &str,
        id: &str,
        result: Result<T, E>,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.log().write(&format!("{what} ({id}): {error}"));
                None
            }
        }
    }

    /// Adds to a step's record, where the caller goes on either way.
    pub(crate) fn record_event(&self, attempt_id: &str, kind: &str, data: &serde_json::Value) {
        if let Err(error) = self.store.append_event(attempt_id, kind, data) {
            self.log().write(&format!(
                "adding \"{kind}\" to a step's record ({attempt_id}): {error}"
            ));
        }
    }

    /// Writes the state a step is in, where the caller goes on either way.
    pub(crate) fn keep_state(
        &self,
        attempt_id: &str,
        state: AttemptState,
        reason: Option<&Reason>,
    ) {
        if let Err(error) = self.store.set_state(attempt_id, state, reason) {
            self.log().write(&format!(
                "keeping that a step is {} ({attempt_id}): {error}",
                state.as_str()
            ));
        }
    }

    /// Writes what a step left, where the caller goes on either way.
    pub(crate) fn keep_fields(&self, attempt_id: &str, fields: &[(&'static str, Value)]) {
        if let Err(error) = self.store.patch_attempt(attempt_id, fields) {
            let names: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
            self.log().write(&format!(
                "keeping a step's {} ({attempt_id}): {error}",
                names.join(", ")
            ));
        }
    }
}
