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

/// Files the user picked to attach; empty when cancelled.
pub async fn pick_files() -> Result<Vec<PathBuf>> {
    let files = rfd::AsyncFileDialog::new().pick_files().await.unwrap_or_default();
    Ok(files.iter().map(|f| f.path().to_path_buf()).collect())
}

/// Where the user wants to save a file suggested as `name`; `None` when cancelled.
pub async fn save_file(name: &str) -> Result<Option<PathBuf>> {
    Ok(rfd::AsyncFileDialog::new().set_file_name(name).save_file().await.map(|f| f.path().to_path_buf()))
}
