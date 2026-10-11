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

/// Saves `data` where the user chooses, suggested as `name`; the saved file's path, or `None` when cancelled.
pub async fn save_file(name: &str, _mime_type: &str, data: Vec<u8>) -> Result<Option<String>> {
    let Some(file) = rfd::AsyncFileDialog::new().set_file_name(name).save_file().await else { return Ok(None) };
    std::fs::write(file.path(), data).with_context(|| format!("saving {}", file.path().display()))?;
    Ok(Some(file.path().display().to_string()))
}

/// Prints `pdf` with the desktop's print dialog (xdg-desktop-portal); without one, the PDF opens
/// in the default viewer to be printed from there. A line for the status bar, `None` if cancelled.
pub async fn print_pdf(title: &str, pdf: Vec<u8>) -> Result<Option<String>> {
    static PRINTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = PRINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("mailclient-print-{}-{n}.pdf", std::process::id()));
    std::fs::write(&path, pdf).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(target_os = "linux")]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (title, file) = (title.to_string(), path.clone());
        // Blocking D-Bus calls on their own thread: the dialog stays open as long as the user likes.
        std::thread::spawn(move || {
            let printed = zbus::blocking::Connection::session().map_err(Into::into).and_then(|bus| portal_print(&bus, &title, &file, n));
            let _ = tx.send(printed);
        });
        match rx.await {
            Ok(Ok(true)) => return Ok(Some("Sent to the printer".into())),
            Ok(Ok(false)) => return Ok(None),
            Ok(Err(e)) => log::info!("no print dialog ({e:#}); opening the PDF instead"),
            Err(_) => log::warn!("print thread ended without an answer"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = title;
    open::that_detached(&path).with_context(|| format!("opening {}", path.display()))?;
    Ok(Some("Opened the message as PDF; print it from there".into()))
}

/// The portal's PreparePrint (the print dialog) and Print. Ok(false) when the user cancelled.
#[cfg(target_os = "linux")]
fn portal_print(bus: &zbus::blocking::Connection, title: &str, path: &std::path::Path, n: u32) -> Result<bool> {
    use std::collections::HashMap;
    use zbus::zvariant::{Fd, OwnedValue, Value};

    const PORTAL: &str = "org.freedesktop.portal.Desktop";
    let portal = zbus::blocking::Proxy::new(bus, PORTAL, "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Print")?;
    let sender = bus.unique_name().context("no D-Bus name")?.trim_start_matches(':').replace('.', "_");
    // A call answers with a Response signal on a request object named after our token; listen first, then call.
    let call = |method: &str, token: &str, body: &dyn Fn(&str) -> Result<()>| -> Result<(u32, HashMap<String, OwnedValue>)> {
        let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        let request = zbus::blocking::Proxy::new(bus, PORTAL, path, "org.freedesktop.portal.Request")?;
        let mut responses = request.receive_signal("Response")?;
        body(token).with_context(|| format!("calling {method}"))?;
        let response = responses.next().context("no response from the print portal")?;
        Ok(response.body().deserialize()?)
    };
    let empty = HashMap::<&str, Value>::new;
    let (code, results) = call("PreparePrint", &format!("mailclient_prepare_{}_{n}", std::process::id()), &|token| {
        let options = HashMap::from([("handle_token", Value::from(token))]);
        portal.call_method("PreparePrint", &("", title, empty(), empty(), options))?;
        Ok(())
    })?;
    match code {
        0 => {}
        1 => return Ok(false),
        _ => anyhow::bail!("print dialog failed ({code})"),
    }
    let token = results.get("token").and_then(|t| u32::try_from(t).ok()).context("print dialog returned no token")?;
    let file = std::fs::File::open(path)?;
    let (code, _) = call("Print", &format!("mailclient_print_{}_{n}", std::process::id()), &|handle| {
        let options = HashMap::from([("token", Value::from(token)), ("handle_token", Value::from(handle))]);
        portal.call_method("Print", &("", title, Fd::from(&file), options))?;
        Ok(())
    })?;
    match code {
        0 => Ok(true),
        1 => Ok(false),
        _ => anyhow::bail!("printing failed ({code})"),
    }
}

/// The portal protocol against a stand-in portal on a private bus (skipped without dbus-daemon).
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, Read};
    use std::sync::{Arc, Mutex};

    use zbus::zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value};

    /// Answers like a real portal whose user confirms the dialog, except for a job titled "cancel".
    struct Portal {
        printed: Arc<Mutex<Vec<(String, u32, Vec<u8>)>>>,
    }

    async fn respond(conn: &zbus::Connection, sender: &str, token: &str, code: u32, results: HashMap<&str, Value<'_>>) -> OwnedObjectPath {
        let path = format!("/org/freedesktop/portal/desktop/request/{}/{token}", sender.trim_start_matches(':').replace('.', "_"));
        conn.emit_signal(Some(sender), path.as_str(), "org.freedesktop.portal.Request", "Response", &(code, results)).await.unwrap();
        OwnedObjectPath::try_from(path).unwrap()
    }

    fn handle_token(options: &HashMap<String, OwnedValue>) -> String {
        String::try_from(options["handle_token"].try_clone().unwrap()).unwrap()
    }

    #[zbus::interface(name = "org.freedesktop.portal.Print")]
    impl Portal {
        async fn prepare_print(
            &self,
            _parent: &str,
            title: &str,
            _settings: HashMap<String, OwnedValue>,
            _page_setup: HashMap<String, OwnedValue>,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] conn: &zbus::Connection,
        ) -> OwnedObjectPath {
            let sender = header.sender().unwrap().to_string();
            let (code, results) =
                if title == "cancel" { (1, HashMap::new()) } else { (0, HashMap::from([("token", Value::from(7u32))])) };
            respond(conn, &sender, &handle_token(&options), code, results).await
        }

        async fn print(
            &self,
            _parent: &str,
            title: &str,
            fd: OwnedFd,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] conn: &zbus::Connection,
        ) -> OwnedObjectPath {
            let mut data = Vec::new();
            std::fs::File::from(std::os::fd::OwnedFd::from(fd)).read_to_end(&mut data).unwrap();
            let token = u32::try_from(&options["token"]).unwrap();
            self.printed.lock().unwrap().push((title.to_string(), token, data));
            respond(conn, &header.sender().unwrap().to_string(), &handle_token(&options), 0, HashMap::new()).await
        }
    }

    #[test]
    fn prints_through_the_portal() {
        let Ok(mut daemon) = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            eprintln!("no dbus-daemon, skipped");
            return;
        };
        let mut address = String::new();
        std::io::BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let address = address.trim();
        let bus = zbus::blocking::connection::Builder::address(address).unwrap().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("m.pdf");
        std::fs::write(&file, b"%PDF-1.7 test").unwrap();
        // Without a portal the caller falls back to the PDF viewer.
        let missing = super::portal_print(&bus, "Report", &file, 0);

        let printed = Arc::new(Mutex::new(Vec::new()));
        let _portal = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .serve_at("/org/freedesktop/portal/desktop", Portal { printed: printed.clone() })
            .unwrap()
            .build()
            .unwrap();
        let result = super::portal_print(&bus, "Report", &file, 1);
        let cancelled = super::portal_print(&bus, "cancel", &file, 2);
        daemon.kill().ok();
        assert!(missing.is_err(), "no portal: {missing:?}");
        assert!(result.unwrap(), "printed");
        assert!(!cancelled.unwrap(), "cancelled");
        let printed = printed.lock().unwrap();
        assert_eq!(*printed, [("Report".to_string(), 7, b"%PDF-1.7 test".to_vec())]);
    }
}
