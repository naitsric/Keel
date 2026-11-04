//! Everything Keel knows about driving Claude Code. See `docs/m0-report.md` for why it
//! looks like this: each rule here comes from something observed in the spike.

pub mod auth;
pub mod command;
pub mod github;
pub mod locate;
pub mod process;
pub mod settings;
pub mod stream;

pub use auth::{AuthStatus, VersionCheck, check_version};
pub use command::{StepCommand, clean_env};
pub use locate::Tools;
pub use stream::{Parsed, parse_line};
