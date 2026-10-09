//! UI-independent mail backend: account config, local cache, MIME handling,
//! and the sync engine with its built-in IMAP/SMTP backend.

pub mod auth;
pub mod backend;
pub mod config;
pub mod engine;
pub mod imap;
pub mod mime;
pub mod smtp;
pub mod store;

pub use backend::Backend;
pub use config::{AccountConfig, Auth, Security};
pub use engine::{Command, Engine, Event};
pub use store::{Folder, MessageSummary, Store};

/// Installs the process-wide rustls crypto provider. Several crates in the
/// dependency tree enable different providers, so rustls cannot pick one itself.
pub fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
