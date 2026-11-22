//! Gateway stores: T1 [`SqliteStore`], its in-process [`backup`] loop, and
//! [`import`] of an existing `graphrt serve --store` directory.

pub mod backup;
pub mod import;
mod sqlite;

pub use sqlite::{DEFAULT_MAX_EVENTS, MAX_PAGE, OutboxEntry, SqliteStore};
