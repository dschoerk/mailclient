use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "mailclient";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Implicit TLS (IMAPS 993, SMTPS 465).
    Tls,
    /// Plain connection upgraded with STARTTLS (IMAP 143, submission 587).
    StartTls,
}

/// How the account logs in. The keyring holds the password or the OAuth refresh token.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Auth {
    #[default]
    Password,
    /// OAuth2 via Microsoft identity (Outlook.com, Microsoft 365).
    Microsoft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    pub name: String,
    pub email: String,
    pub username: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    #[serde(default)]
    pub auth: Auth,
}

/// App preferences, stored next to the accounts in `config.toml`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Puts the phone reader's thumb actions on the left.
    pub left_handed: bool,
    /// Phone reader actions float over the message instead of sitting in a bottom bar.
    pub floating_buttons: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(flatten)]
    settings: Settings,
    #[serde(default)]
    accounts: Vec<AccountConfig>,
}

impl AccountConfig {
    /// Best-effort server settings for a mail address, used to prefill the setup form.
    pub fn guess(email: &str) -> AccountConfig {
        let domain = email.rsplit_once('@').map(|(_, d)| d.to_lowercase()).unwrap_or_default();
        let microsoft = matches!(domain.split('.').next(), Some("outlook" | "hotmail" | "live" | "msn"));
        let (imap, smtp, smtp_port, smtp_security) = match domain.as_str() {
            "gmail.com" | "googlemail.com" => ("imap.gmail.com", "smtp.gmail.com", 465, Security::Tls),
            _ if microsoft => ("outlook.office365.com", "smtp.office365.com", 587, Security::StartTls),
            "fastmail.com" | "fastmail.fm" => ("imap.fastmail.com", "smtp.fastmail.com", 465, Security::Tls),
            "icloud.com" | "me.com" => ("imap.mail.me.com", "smtp.mail.me.com", 587, Security::StartTls),
            "gmx.net" | "gmx.at" | "gmx.de" => ("imap.gmx.net", "mail.gmx.net", 587, Security::StartTls),
            _ => ("", "", 465, Security::Tls),
        };
        let or_prefixed =
            |host: &str, prefix: &str| if host.is_empty() { format!("{prefix}.{domain}") } else { host.to_string() };
        AccountConfig {
            name: email.split('@').next().unwrap_or_default().to_string(),
            email: email.to_string(),
            username: email.to_string(),
            imap_host: or_prefixed(imap, "imap"),
            imap_port: 993,
            imap_security: Security::Tls,
            smtp_host: or_prefixed(smtp, "smtp"),
            smtp_port,
            smtp_security,
            auth: if microsoft { Auth::Microsoft } else { Auth::Password },
        }
    }

    #[cfg(not(target_os = "android"))]
    fn keyring_entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("{}@{}", self.username, self.imap_host))
            .context("opening system keyring")
    }

    /// Android: Keystore-backed store; `keyring`'s v1 mode has none for Android.
    #[cfg(target_os = "android")]
    fn keyring_entry(&self) -> Result<keyring_core::Entry> {
        static STORE: std::sync::Once = std::sync::Once::new();
        STORE.call_once(|| match android_native_keyring_store::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(e) => log::error!("Android keystore unavailable: {e}"),
        });
        keyring_core::Entry::new(KEYRING_SERVICE, &format!("{}@{}", self.username, self.imap_host))
            .context("opening Android keystore")
    }

    /// Password or OAuth refresh token, from `MAILCLIENT_PASSWORD` (for development) or the system keyring.
    pub fn password(&self) -> Result<String> {
        if let Ok(pw) = std::env::var("MAILCLIENT_PASSWORD") {
            return Ok(pw);
        }
        self.keyring_entry()?
            .get_password()
            .map_err(|e| anyhow!("no password in keyring for {}: {e}", self.email))
    }

    pub fn store_password(&self, password: &str) -> Result<()> {
        if std::env::var_os("MAILCLIENT_PASSWORD").is_some() {
            return Ok(()); // the override replaces the keyring for writes too (tests, development)
        }
        self.keyring_entry()?.set_password(password).context("saving password to keyring")
    }
}

static BASE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Overrides where config and data live; for platforms without a home directory
/// (Android passes the app's internal storage).
pub fn set_base_dir(dir: PathBuf) {
    let _ = BASE_DIR.set(dir);
}

pub fn config_dir() -> Result<PathBuf> {
    if let Some(base) = BASE_DIR.get() {
        return Ok(base.join("config"));
    }
    let dirs = directories::ProjectDirs::from("", "", "mailclient")
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(dirs.config_dir().to_path_buf())
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(base) = BASE_DIR.get() {
        return Ok(base.join("data"));
    }
    let dirs = directories::ProjectDirs::from("", "", "mailclient")
        .ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(dirs.data_dir().to_path_buf())
}

fn read_config() -> Result<ConfigFile> {
    let path = config_dir()?.join("config.toml");
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn write_config(file: &ConfigFile) -> Result<()> {
    let dir = config_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("config.toml"), toml::to_string_pretty(file)?)?;
    Ok(())
}

/// v0.1 supports a single account: the first one in `config.toml`.
pub fn load_account() -> Result<Option<AccountConfig>> {
    Ok(read_config()?.accounts.into_iter().next())
}

pub fn save_account(account: &AccountConfig) -> Result<()> {
    // An unreadable file must not block setup; it gets replaced as before.
    let mut file = read_config().unwrap_or_default();
    file.accounts = vec![account.clone()];
    write_config(&file)
}

pub fn load_settings() -> Result<Settings> {
    Ok(read_config()?.settings)
}

pub fn save_settings(settings: Settings) -> Result<()> {
    let mut file = read_config()?;
    file.settings = settings;
    write_config(&file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_known_and_unknown_providers() {
        let gmail = AccountConfig::guess("someone@gmail.com");
        assert_eq!(gmail.imap_host, "imap.gmail.com");
        assert_eq!(gmail.smtp_port, 465);
        assert_eq!(gmail.auth, Auth::Password);
        let hotmail = AccountConfig::guess("someone@hotmail.co.uk");
        assert_eq!((hotmail.imap_host.as_str(), hotmail.auth), ("outlook.office365.com", Auth::Microsoft));

        let other = AccountConfig::guess("me@example.org");
        assert_eq!(other.imap_host, "imap.example.org");
        assert_eq!(other.smtp_host, "smtp.example.org");
        assert_eq!(other.username, "me@example.org");
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let account = AccountConfig::guess("me@example.org");
        let settings = Settings { left_handed: true, floating_buttons: true };
        let text = toml::to_string_pretty(&ConfigFile { accounts: vec![account.clone()], settings }).unwrap();
        let parsed: ConfigFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.accounts, vec![account]);
        assert_eq!(parsed.settings, settings);
        // Files written before a setting existed still load.
        let old: ConfigFile = toml::from_str("left_handed = true").unwrap();
        assert_eq!(old.settings, Settings { left_handed: true, floating_buttons: false });
    }
}
