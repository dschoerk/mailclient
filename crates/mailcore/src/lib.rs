//! UI-independent mail backend: account config, local cache, MIME handling,
//! and the IMAP/SMTP sync engine.

pub mod config;
pub mod engine;
pub mod imap;
pub mod mime;
pub mod smtp;
pub mod store;

pub use config::{AccountConfig, Security};
pub use engine::{Command, Engine, Event};
pub use store::{Folder, MessageSummary, Store};

/// Installs the process-wide rustls crypto provider. Several crates in the
/// dependency tree enable different providers, so rustls cannot pick one itself.
pub fn init_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
