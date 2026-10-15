//! Adding, editing and removing accounts: the setup form and their engines.

use super::*;

impl Controller {
    // ---- setup ----

    pub(super) fn setup_email_edited(&self, email: &str) {
        let window = self.window();
        let mut data = window.get_setup();
        let guess = AccountConfig::guess(email);
        data.username = guess.username.into();
        data.imap_host = guess.imap_host.into();
        data.imap_port = guess.imap_port.to_string().into();
        data.imap_starttls = guess.imap_security == Security::StartTls;
        data.smtp_host = guess.smtp_host.into();
        data.smtp_port = guess.smtp_port.to_string().into();
        data.smtp_starttls = guess.smtp_security == Security::StartTls;
        data.caldav_url = guess.caldav_url.into();
        // Builds without the provider's client ID fall back to a (app) password.
        data.oauth = mailcore::auth::available(guess.auth);
        // Follow the address while the name still looks auto-filled (typing arrives per character).
        let name = data.name.to_string();
        if name.is_empty() || guess.name.starts_with(&name) || name.starts_with(&guess.name) {
            data.name = guess.name.into();
        }
        window.set_setup(data);
    }

    pub(super) fn account_from_setup(data: &SetupData) -> Result<AccountConfig> {
        let port = |s: &SharedString, what: &str| s.trim().parse::<u16>().map_err(|_| anyhow!("invalid {what} port"));
        let security = |starttls: bool| if starttls { Security::StartTls } else { Security::Tls };
        let account = AccountConfig {
            name: data.name.trim().to_string(),
            email: data.email.trim().to_string(),
            username: data.username.trim().to_string(),
            imap_host: data.imap_host.trim().to_string(),
            imap_port: port(&data.imap_port, "IMAP")?,
            imap_security: security(data.imap_starttls),
            smtp_host: data.smtp_host.trim().to_string(),
            smtp_port: port(&data.smtp_port, "SMTP")?,
            smtp_security: security(data.smtp_starttls),
            // shortcut: a ticked box on an unknown domain means Microsoft 365; Google Workspace custom domains need their own choice.
            auth: match AccountConfig::guess(&data.email).auth {
                _ if !data.oauth => Auth::Password,
                Auth::Password => Auth::Microsoft,
                auth => auth,
            },
            caldav_url: if data.oauth { String::new() } else { data.caldav_url.trim().to_string() },
            // Chosen in Settings, not in the form: `setup_submit` keeps an edited account's.
            color: String::new(),
        };
        if !account.email.contains('@') {
            return Err(anyhow!("enter a valid email address"));
        }
        if account.imap_host.is_empty() || account.smtp_host.is_empty() {
            return Err(anyhow!("enter the IMAP and SMTP servers"));
        }
        Ok(account)
    }

    /// Opens the setup form on an existing account.
    pub(super) fn edit_account(&self, index: usize) {
        let Some(account) = self.state.borrow().accounts.get(index).map(|a| a.config.clone()) else { return };
        self.state.borrow_mut().editing = Some(index);
        let window = self.window();
        window.set_setup(SetupData {
            name: account.name.into(),
            email: account.email.into(),
            username: account.username.into(),
            password: "".into(),
            imap_host: account.imap_host.into(),
            imap_port: account.imap_port.to_string().into(),
            imap_starttls: account.imap_security == Security::StartTls,
            smtp_host: account.smtp_host.into(),
            smtp_port: account.smtp_port.to_string().into(),
            smtp_starttls: account.smtp_security == Security::StartTls,
            oauth: account.auth != Auth::Password,
            caldav_url: account.caldav_url.into(),
        });
        window.set_setup_error("".into());
        window.set_setup_editing(true);
        window.set_needs_setup(true);
    }

    pub(super) fn setup_submit(&self) {
        let window = self.window();
        let data = window.get_setup();
        let mut account = match Self::account_from_setup(&data) {
            Ok(a) => a,
            Err(e) => return window.set_setup_error(e.to_string().into()),
        };
        let editing = self.state.borrow().editing;
        let old = editing.and_then(|i| self.state.borrow().accounts.get(i).map(|a| a.config.clone()));
        if let Some(old) = &old {
            account.color = old.color.clone();
        }
        if old.is_none() && self.state.borrow().accounts.iter().any(|a| a.config.email.eq_ignore_ascii_case(&account.email)) {
            return window.set_setup_error("This account is already set up".into());
        }
        if old.is_none() && account.auth == Auth::Password && data.password.is_empty() {
            return window.set_setup_error("enter your password".into());
        }
        window.set_setup_error("".into());
        window.set_setup_busy(true);
        let password = data.password.to_string();
        // Verify the login before saving anything.
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| {
                    rt.block_on(async {
                        // (secret to log in with now, secret to keep in the keyring)
                        let (login, keep) = match account.auth {
                            Auth::Password if !password.is_empty() => (password.clone(), password),
                            // Editing: keep the stored secret; an expired sign-in falls back to the browser.
                            _ => match stored_secret(old.as_ref(), account.auth).await {
                                Ok(secrets) => secrets,
                                Err(e) if account.auth == Auth::Password => Err(e.context("enter your password"))?,
                                Err(e) => {
                                    log::info!("signing in again: {e:#}");
                                    browser_sign_in(&account).await?
                                }
                            },
                        };
                        let imap = mailcore::imap::Imap::connect(&account, &login).await?;
                        imap.logout().await;
                        anyhow::Ok(keep)
                    })
                })
                .and_then(|keep| account.store_password(&keep))
                .and_then(|()| config::save_account(&account));
            if result.is_ok()
                && let Some(old) = old.filter(|old| (&old.username, &old.imap_host) != (&account.username, &account.imap_host))
                && let Err(e) = old.delete_password()
            {
                log::warn!("{e:#}"); // the secret moved to a new keyring entry; the old one is only clutter
            }
            with_controller(move |c| {
                let window = c.window();
                window.set_setup_busy(false);
                let result = result.and_then(|()| match editing {
                    Some(index) => c.replace_account(index, account),
                    None => c.start(account),
                });
                match result {
                    Ok(()) => {
                        c.state.borrow_mut().editing = None;
                        window.set_needs_setup(false);
                        window.set_setup(SetupData::default());
                    }
                    Err(e) => window.set_setup_error(format!("{e:#}").into()),
                }
            });
        });
    }

    /// Opens the account's cache and starts its engine.
    fn open_account(&self, config: AccountConfig) -> Result<Account> {
        let db = db_path(&config)?;
        let store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;
        let id = self.next_account_id.replace(self.next_account_id.get() + 1);
        let engine = Engine::start(config.clone(), db, move |event| with_controller(move |c| c.on_event(id, event)));
        Ok(Account { id, config, engine, store, folders: Vec::new(), online: true, push: false, error: None })
    }

    pub(super) fn start(&self, account: AccountConfig) -> Result<()> {
        let account = self.open_account(account)?;
        {
            let mut state = self.state.borrow_mut();
            state.accounts.push(account);
            // Back to the default view: the unified inbox once there are several accounts.
            state.current = None;
        }
        self.refresh_accounts();
        // Show the cache immediately; the engine syncs in the background.
        self.reload_folders();
        Ok(())
    }

    /// Restarts an edited account's engine with its new settings.
    fn replace_account(&self, index: usize, config: AccountConfig) -> Result<()> {
        let account = self.open_account(config)?;
        let open_folder = {
            let mut state = self.state.borrow_mut();
            // Dropping the old account stops its engine.
            state.accounts[index] = account;
            state.fetching.retain(|(a, _, _)| *a != index);
            match &state.current {
                Some(Row::Folder(a, name)) if *a == index => Some(name.clone()),
                _ => None,
            }
        };
        if let Some(folder) = open_folder {
            self.send(index, Command::OpenFolder(folder));
        }
        self.refresh_accounts();
        self.reload_folders();
        Ok(())
    }

    /// Stops the account and forgets it on this device: config, keyring entry and cache.
    pub(super) fn remove_account(&self, index: usize) {
        if index >= self.state.borrow().accounts.len() || self.window().get_composing() {
            return;
        }
        self.close_message();
        let config = {
            let mut state = self.state.borrow_mut();
            let account = state.accounts.remove(index);
            // Rows and messages are keyed by account index, which just shifted.
            state.current = None;
            state.checked.clear();
            state.remote_allowed.clear();
            state.fetching.clear();
            state.messages.clear();
            state.conversations.clear();
            account.config
        };
        let db = db_path(&config);
        let result = config::remove_account(&config.email).and_then(|()| config.delete_password()).and_then(|()| {
            // shortcut: the stopping engine thread may still hold the file open, which blocks deleting it on Windows; delete on next start if that matters.
            let db = db?;
            for suffix in ["", "-wal", "-shm"] {
                let mut path = db.clone().into_os_string();
                path.push(suffix);
                let path = std::path::PathBuf::from(path);
                if let Err(e) = std::fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    log::warn!("removing {}: {e}", path.display());
                }
            }
            Ok(())
        });
        if let Err(e) = result {
            self.show_error(&e);
        }
        self.message_model.set_vec(Vec::new());
        self.refresh_accounts();
        self.reload_folders();
        if self.state.borrow().accounts.is_empty() {
            let window = self.window();
            window.set_show_settings(false);
            window.set_setup(SetupData::default());
            window.set_setup_editing(false);
            window.set_needs_setup(true);
        }
    }

    /// Settings: the account's color becomes `palette` entry `choice`.
    pub(super) fn set_account_color(&self, index: usize, choice: usize) {
        let config = {
            let mut state = self.state.borrow_mut();
            let Some(account) = state.accounts.get_mut(index) else { return };
            account.config.color = format!("#{:06x}", folders::ACCOUNT_COLORS[choice % folders::ACCOUNT_COLORS.len()]);
            account.config.clone()
        };
        if let Err(e) = config::save_account(&config) {
            self.show_error(&e);
        }
        self.refresh_accounts();
        self.reload_folders();
        self.reload_messages();
        self.render_calendar();
    }

    /// Shows the accounts in Settings and the sidebar label.
    fn refresh_accounts(&self) {
        let state = self.state.borrow();
        let items: Vec<AccountItem> = state
            .accounts
            .iter()
            .enumerate()
            .map(|(i, a)| AccountItem {
                name: a.config.name.clone().into(),
                email: a.config.email.clone().into(),
                color: folders::account_color(&state.accounts, i, true),
            })
            .collect();
        let label = match state.accounts.as_slice() {
            [] => String::new(),
            [only] => format::person(&only.config.name, &only.config.email),
            all => format!("{} accounts", all.len()),
        };
        let window = self.window();
        window.set_accounts(ModelRc::new(VecModel::from(items)));
        window.set_account_label(label.into());
        window.set_has_account(!state.accounts.is_empty());
    }
}

pub(crate) fn db_path(account: &AccountConfig) -> Result<std::path::PathBuf> {
    Ok(config::data_dir()?.join(format!("{}.sqlite", account.email)))
}

/// The stored secret of the account being edited: (to log in with now, to keep).
async fn stored_secret(old: Option<&AccountConfig>, auth: Auth) -> Result<(String, String)> {
    let old = old.filter(|old| old.auth == auth).ok_or_else(|| anyhow!("no stored sign-in"))?;
    // Refreshing may rotate the refresh token, so read the keyring after it.
    let creds = mailcore::auth::Credentials::load(old)?;
    let login = creds.secret().await?;
    // Sign-ins from before the calendar lack its permission: asking again adds it.
    if auth != Auth::Password {
        creds.calendar_token().await?;
    }
    Ok((login, old.password()?))
}

/// Browser sign-in: (access token, refresh token).
async fn browser_sign_in(account: &AccountConfig) -> Result<(String, String)> {
    let sign_in = mailcore::auth::SignIn::start(account.auth, &account.email).await?;
    crate::platform::open_url(&sign_in.url)?;
    let tokens = tokio::time::timeout(std::time::Duration::from_secs(300), sign_in.finish())
        .await
        .map_err(|_| anyhow!("sign-in timed out; try again"))??;
    let refresh = tokens.refresh.ok_or_else(|| anyhow!("the sign-in returned no refresh token"))?;
    Ok((tokens.access, refresh))
}
