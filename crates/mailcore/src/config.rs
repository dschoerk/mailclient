use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[cfg(not(target_os = "android"))]
use keyring::Error as KeyringError;
#[cfg(target_os = "android")]
use keyring_core::Error as KeyringError;

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
    /// OAuth2 via Google (Gmail).
    Google,
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
    /// CalDAV server or calendar URL for password accounts; Google and Microsoft accounts find theirs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caldav_url: String,
    /// The account's color in the lists, `#rrggbb`; empty: one picked by its place.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follows the system's light or dark mode.
    #[default]
    System,
    Light,
    Dark,
}

/// App preferences, stored next to the accounts in `config.toml`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Puts the phone reader's thumb actions on the left.
    pub left_handed: bool,
    /// Phone reader actions float over the message instead of sitting in a bottom bar.
    pub floating_buttons: bool,
    pub theme: Theme,
    /// No notification for new mail.
    pub mute_notifications: bool,
    /// Remote images load without asking (gives up tracking protection).
    pub load_remote_images: bool,
    /// HTML mail shows the host each link goes to next to it, flagging links whose text names another site.
    pub show_link_targets: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigFile {
    #[serde(flatten)]
    settings: Settings,
    #[serde(default)]
    accounts: Vec<AccountConfig>,
}

impl ConfigFile {
    fn upsert(&mut self, account: &AccountConfig) {
        match self
            .accounts
            .iter_mut()
            .find(|a| a.email.eq_ignore_ascii_case(&account.email))
        {
            Some(existing) => *existing = account.clone(),
            None => self.accounts.push(account.clone()),
        }
    }

    fn remove(&mut self, email: &str) {
        self.accounts.retain(|a| !a.email.eq_ignore_ascii_case(email));
    }
}

impl AccountConfig {
    /// Gmail: folders are labels, and All Mail holds every message once.
    pub fn is_gmail(&self) -> bool {
        self.imap_host.to_lowercase().ends_with("gmail.com")
    }

    /// Best-effort server settings for a mail address, used to prefill the setup form.
    pub fn guess(email: &str) -> AccountConfig {
        let domain = email
            .rsplit_once('@')
            .map(|(_, d)| d.to_lowercase())
            .unwrap_or_default();
        let microsoft = matches!(
            domain.split('.').next(),
            Some("outlook" | "hotmail" | "live" | "msn")
        );
        let google = matches!(domain.as_str(), "gmail.com" | "googlemail.com");
        let (imap, smtp, smtp_port, smtp_security) = match domain.as_str() {
            "gmail.com" | "googlemail.com" => {
                ("imap.gmail.com", "smtp.gmail.com", 465, Security::Tls)
            }
            // Outlook.com's own submission server (support.microsoft.com, "POP, IMAP, and SMTP
            // settings for Outlook.com"); Microsoft 365 domains use smtp.office365.com.
            _ if microsoft => (
                "outlook.office365.com",
                "smtp-mail.outlook.com",
                587,
                Security::StartTls,
            ),
            "fastmail.com" | "fastmail.fm" => {
                ("imap.fastmail.com", "smtp.fastmail.com", 465, Security::Tls)
            }
            "icloud.com" | "me.com" => (
                "imap.mail.me.com",
                "smtp.mail.me.com",
                587,
                Security::StartTls,
            ),
            "gmx.net" | "gmx.at" | "gmx.de" => {
                ("imap.gmx.net", "mail.gmx.net", 587, Security::StartTls)
            }
            _ => ("", "", 465, Security::Tls),
        };
        let caldav = match domain.as_str() {
            "fastmail.com" | "fastmail.fm" => "https://caldav.fastmail.com/",
            "icloud.com" | "me.com" => "https://caldav.icloud.com/",
            "mailbox.org" => "https://dav.mailbox.org/",
            _ => "",
        };
        let or_prefixed = |host: &str, prefix: &str| {
            if host.is_empty() {
                format!("{prefix}.{domain}")
            } else {
                host.to_string()
            }
        };
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
            auth: if microsoft {
                Auth::Microsoft
            } else if google {
                Auth::Google
            } else {
                Auth::Password
            },
            caldav_url: caldav.to_string(),
            color: String::new(),
        }
    }

    #[cfg(not(target_os = "android"))]
    fn keyring_entry(&self) -> Result<keyring::Entry> {
        keyring::Entry::new(
            KEYRING_SERVICE,
            &format!("{}@{}", self.username, self.imap_host),
        )
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
        keyring_core::Entry::new(
            KEYRING_SERVICE,
            &format!("{}@{}", self.username, self.imap_host),
        )
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
        self.keyring_entry()?
            .set_password(password)
            .context("saving password to keyring")
    }

    /// Forgets the stored password or refresh token; nothing stored is fine.
    pub fn delete_password(&self) -> Result<()> {
        if std::env::var_os("MAILCLIENT_PASSWORD").is_some() {
            return Ok(());
        }
        match self.keyring_entry()?.delete_credential() {
            Err(KeyringError::NoEntry) => Ok(()),
            result => result.context("removing password from keyring"),
        }
    }
}

static BASE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static CONFIG_FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Uses `path` instead of `config.toml` in the config directory (`mailclient --config <file>`).
pub fn set_config_file(path: PathBuf) {
    let _ = CONFIG_FILE.set(path);
}

fn config_file() -> Result<PathBuf> {
    match CONFIG_FILE.get() {
        Some(path) => Ok(path.clone()),
        None => Ok(config_dir()?.join("config.toml")),
    }
}

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
    let path = config_file()?;
    if !path.exists() {
        return Ok(ConfigFile::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn write_config(file: &ConfigFile) -> Result<()> {
    let path = config_file()?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(file)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// All accounts, in the order they were added.
pub fn load_accounts() -> Result<Vec<AccountConfig>> {
    Ok(read_config()?.accounts)
}

/// Adds `account`, or replaces the one with the same address.
pub fn save_account(account: &AccountConfig) -> Result<()> {
    // An unreadable file must not block setup; it gets replaced as before.
    let mut file = read_config().unwrap_or_default();
    file.upsert(account);
    write_config(&file)
}

/// Removes the account with this address.
pub fn remove_account(email: &str) -> Result<()> {
    let mut file = read_config()?;
    file.remove(email);
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
        assert_eq!(gmail.auth, Auth::Google);
        let hotmail = AccountConfig::guess("someone@hotmail.co.uk");
        assert_eq!(
            (hotmail.imap_host.as_str(), hotmail.smtp_host.as_str(), hotmail.auth),
            ("outlook.office365.com", "smtp-mail.outlook.com", Auth::Microsoft)
        );

        let other = AccountConfig::guess("me@example.org");
        assert_eq!(other.imap_host, "imap.example.org");
        assert_eq!(other.smtp_host, "smtp.example.org");
        assert_eq!(other.username, "me@example.org");
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let account = AccountConfig::guess("me@example.org");
        let settings = Settings {
            left_handed: true,
            floating_buttons: true,
            theme: Theme::Dark,
            mute_notifications: true,
            load_remote_images: true,
            show_link_targets: true,
        };
        let text = toml::to_string_pretty(&ConfigFile {
            accounts: vec![account.clone()],
            settings,
        })
        .unwrap();
        let parsed: ConfigFile = toml::from_str(&text).unwrap();
        assert_eq!(parsed.accounts, vec![account]);
        assert_eq!(parsed.settings, settings);
        // Files written before a setting existed still load.
        let old: ConfigFile = toml::from_str("left_handed = true").unwrap();
        assert_eq!(
            old.settings,
            Settings {
                left_handed: true,
                ..Settings::default()
            }
        );
    }

    #[test]
    fn saving_adds_accounts_and_replaces_by_address() {
        let mut file = ConfigFile::default();
        file.upsert(&AccountConfig::guess("a@example.org"));
        file.upsert(&AccountConfig::guess("b@gmail.com"));
        let mut changed = AccountConfig::guess("A@example.org");
        changed.name = "Changed".into();
        file.upsert(&changed);
        let emails: Vec<&str> = file.accounts.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["A@example.org", "b@gmail.com"]);
        assert_eq!(file.accounts[0].name, "Changed");
        file.remove("a@EXAMPLE.org");
        let emails: Vec<&str> = file.accounts.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["b@gmail.com"]);
    }
}
