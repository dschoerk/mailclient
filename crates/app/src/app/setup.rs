//! Adding an account: the setup form and starting its engine.

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
        data.oauth = guess.auth != Auth::Password;
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
            auth: if data.oauth { Auth::Microsoft } else { Auth::Password },
        };
        if !account.email.contains('@') {
            return Err(anyhow!("enter a valid email address"));
        }
        if account.imap_host.is_empty() || account.smtp_host.is_empty() {
            return Err(anyhow!("enter the IMAP and SMTP servers"));
        }
        if account.auth == Auth::Password && data.password.is_empty() {
            return Err(anyhow!("enter your password"));
        }
        Ok(account)
    }

    pub(super) fn setup_submit(&self) {
        let window = self.window();
        let data = window.get_setup();
        let account = match Self::account_from_setup(&data) {
            Ok(a) => a,
            Err(e) => return window.set_setup_error(e.to_string().into()),
        };
        if self.state.borrow().accounts.iter().any(|a| a.config.email.eq_ignore_ascii_case(&account.email)) {
            return window.set_setup_error("This account is already set up".into());
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
                            Auth::Password => (password.clone(), password),
                            Auth::Microsoft => {
                                let sign_in = mailcore::auth::SignIn::start(&account.email).await?;
                                crate::platform::open_url(&sign_in.url)?;
                                let tokens = tokio::time::timeout(std::time::Duration::from_secs(300), sign_in.finish())
                                    .await
                                    .map_err(|_| anyhow!("sign-in timed out; try again"))??;
                                let refresh = tokens.refresh.ok_or_else(|| anyhow!("Microsoft returned no refresh token"))?;
                                (tokens.access, refresh)
                            }
                        };
                        let imap = mailcore::imap::Imap::connect(&account, &login).await?;
                        imap.logout().await;
                        anyhow::Ok(keep)
                    })
                })
                .and_then(|keep| account.store_password(&keep))
                .and_then(|()| config::save_account(&account));
            with_controller(move |c| {
                let window = c.window();
                window.set_setup_busy(false);
                match result.and_then(|()| c.start(account)) {
                    Ok(()) => {
                        window.set_needs_setup(false);
                        window.set_setup(SetupData::default());
                    }
                    Err(e) => window.set_setup_error(format!("{e:#}").into()),
                }
            });
        });
    }

    pub(super) fn start(&self, account: AccountConfig) -> Result<()> {
        let db = config::data_dir()?.join(format!("{}.sqlite", account.email));
        let store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;
        let index = self.state.borrow().accounts.len();
        let engine = Engine::start(account.clone(), db, move |event| with_controller(move |c| c.on_event(index, event)));
        let label = {
            let mut state = self.state.borrow_mut();
            state.accounts.push(Account { config: account, engine, store, folders: Vec::new(), online: true });
            // Back to the default view: the unified inbox once there are several accounts.
            state.current = None;
            match state.accounts.as_slice() {
                [only] => format::person(&only.config.name, &only.config.email),
                all => format!("{} accounts", all.len()),
            }
        };
        self.window().set_account_label(label.into());
        self.window().set_has_account(true);
        // Show the cache immediately; the engine syncs in the background.
        self.reload_folders();
        Ok(())
    }
}
