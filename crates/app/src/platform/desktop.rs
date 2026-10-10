use std::path::PathBuf;

use anyhow::{Context, Result};

pub fn init_logging() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,mailclient_app=info,mailcore=info"))
        .init();
}

/// Opens `url` in the system browser.
pub fn open_url(url: &str) -> Result<()> {
    open::that_detached(url).with_context(|| format!("opening {url}"))
}

#[cfg(test)]
thread_local! {
    /// Notifications shown during a test, instead of going to the desktop.
    pub static NOTIFIED: std::cell::RefCell<Vec<(String, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Shows a desktop notification (freedesktop Notifications over D-Bus on Linux).
pub fn notify(title: &str, body: &str) {
    #[cfg(test)]
    return NOTIFIED.with(|n| n.borrow_mut().push((title.into(), body.into())));
    #[cfg(all(target_os = "linux", not(test)))]
    {
        let (title, body) = (title.to_string(), body.to_string());
        // A blocking call on its own thread: the bus may be slow or missing.
        std::thread::spawn(move || {
            let result = zbus::blocking::Connection::session().and_then(|bus| {
                let hints = std::collections::HashMap::<&str, zbus::zvariant::Value>::new();
                bus.call_method(
                    Some("org.freedesktop.Notifications"),
                    "/org/freedesktop/Notifications",
                    Some("org.freedesktop.Notifications"),
                    "Notify",
                    &("Mail", 0u32, "mail-unread", title, body, Vec::<&str>::new(), hints, -1i32),
                )
            });
            if let Err(e) = result {
                log::warn!("cannot show notification: {e}");
            }
        });
    }
}

/// Files the user picked to attach; empty when cancelled.
pub async fn pick_files() -> Result<Vec<PathBuf>> {
    let files = rfd::AsyncFileDialog::new().pick_files().await.unwrap_or_default();
    Ok(files.iter().map(|f| f.path().to_path_buf()).collect())
}

/// Where the user wants to save a file suggested as `name`; `None` when cancelled.
pub async fn save_file(name: &str) -> Result<Option<PathBuf>> {
    Ok(rfd::AsyncFileDialog::new().set_file_name(name).save_file().await.map(|f| f.path().to_path_buf()))
}
