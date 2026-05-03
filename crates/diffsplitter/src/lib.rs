//! Diffsplitter library.
//!
//! The library form is exported so `tests/` can build against the same modules
//! the binary uses.

pub mod config;
pub mod db;
pub mod diff;
pub mod metrics;
pub mod proxy;
pub mod worker;

use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

/// SQLite "pool" — a single-writer Mutex<Connection>. SQLite's WAL mode lets
/// readers run lock-free, but writers serialize. For our QPS that is fine and
/// simpler than r2d2 + busy_timeout retries.
pub type DbPool = Arc<Mutex<Connection>>;

/// Shared application state. Cloned cheaply (Arc internally).
pub struct State {
    pub cfg: config::Config,
    pub http: reqwest::Client,
    pub pool: DbPool,
}
