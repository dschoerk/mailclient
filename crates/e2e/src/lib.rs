//! Drives a running mailclient through Slint's embedded MCP server (JSON-RPC over HTTP),
//! on the desktop or on an Android device/emulator. Select with `E2E_TARGET=android`.
//!
//! The app must be built with `--features slint/mcp` (see README, "End-to-end tests").

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use mailcore::imap::Imap;
use mailcore::mime::{self, Draft};
use mailcore::{AccountConfig, Security};
use serde_json::{Value, json};

pub const PACKAGE: &str = "dev.mailclient.app";

pub fn workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// CA of the GreenMail server started by scripts/test-mailserver.sh.
pub fn test_ca() -> PathBuf {
    workspace_dir().join("target/test-mailserver/ca.pem")
}

pub fn android() -> bool {
    std::env::var("E2E_TARGET").is_ok_and(|t| t == "android")
}

fn adb_command() -> Command {
    match std::env::var_os("ANDROID_HOME") {
        Some(sdk) => Command::new(PathBuf::from(sdk).join("platform-tools/adb")),
        None => Command::new("adb"),
    }
}

fn adb(args: &[&str]) {
    let status = adb_command().args(args).status().expect("adb not found");
    assert!(status.success(), "adb {args:?} failed");
}

/// A running app with a fresh, empty configuration.
pub struct App {
    port: u16,
    child: Option<Child>,
    window: Value,
    _data: Option<PathBuf>,
}

impl App {
    pub fn launch() -> App {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut app = if android() { Self::launch_android(port) } else { Self::launch_desktop(port) };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(windows) = app.call("list_windows", json!({}))
                && let Some(w) = windows["windowHandles"].get(0)
            {
                app.window = w.clone();
                return app;
            }
            assert!(Instant::now() < deadline, "app did not come up on port {port}");
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn launch_desktop(port: u16) -> App {
        let bin = std::env::var_os("E2E_APP").map(PathBuf::from).unwrap_or(workspace_dir().join("target/debug/mailclient"));
        assert!(bin.exists(), "{} missing: cargo build -p mailclient --features slint/mcp", bin.display());
        let data = std::env::temp_dir().join(format!("mailclient-e2e-{port}"));
        let child = Command::new(bin)
            .env("SLINT_MCP_PORT", port.to_string())
            .env("XDG_CONFIG_HOME", data.join("config"))
            .env("XDG_DATA_HOME", data.join("data"))
            .env("MAILCLIENT_PASSWORD", "x") // keeps the test out of the real keyring
            .env("MAILCLIENT_EXTRA_CA", test_ca())
            .spawn()
            .unwrap();
        App { port, child: Some(child), window: Value::Null, _data: Some(data) }
    }

    /// Expects the APK to be installed. Wipes its data, so every run starts at setup.
    fn launch_android(port: u16) -> App {
        let p = &port.to_string();
        adb(&["shell", "am", "force-stop", PACKAGE]);
        adb(&["shell", "pm", "clear", PACKAGE]);
        // Answers the permission prompt the app shows on first start (US19).
        adb(&["shell", "pm", "grant", PACKAGE, "android.permission.POST_NOTIFICATIONS"]);
        adb(&["push", test_ca().to_str().unwrap(), "/data/local/tmp/mailclient-ca.pem"]);
        // run-as only works for debug builds, which is what tests use.
        let setup = format!("mkdir -p files && cp /data/local/tmp/mailclient-ca.pem files/extra-ca.pem && echo {port} > files/mcp-port");
        adb(&["shell", "run-as", PACKAGE, "sh", "-c", &format!("'{setup}'")]);
        // The app reaches GreenMail as localhost, matching the test certificate.
        adb(&["reverse", "tcp:3993", "tcp:3993"]);
        adb(&["reverse", "tcp:3465", "tcp:3465"]);
        adb(&["forward", &format!("tcp:{p}"), &format!("tcp:{p}")]);
        adb(&["shell", "am", "start", "-n", &format!("{PACKAGE}/android.app.NativeActivity")]);
        App { port, child: None, window: Value::Null, _data: None }
    }

    /// Calls an MCP tool and returns its JSON result.
    pub fn call(&self, tool: &str, arguments: Value) -> Result<Value, String> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": arguments}})
            .to_string();
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(
            stream,
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .map_err(|e| e.to_string())?;
        let mut response = String::new();
        stream.read_to_string(&mut response).map_err(|e| e.to_string())?;
        let (_, payload) = response.split_once("\r\n\r\n").ok_or("malformed HTTP response")?;
        let reply: Value = serde_json::from_str(payload).map_err(|e| format!("{e}: {payload}"))?;
        let result = &reply["result"];
        let first = &result["content"][0];
        if result["isError"].as_bool() == Some(true) {
            return Err(first["text"].as_str().unwrap_or_default().to_string());
        }
        match first["type"].as_str() {
            Some("image") => Ok(first.clone()),
            _ => serde_json::from_str(first["text"].as_str().unwrap_or("null")).map_err(|e| e.to_string()),
        }
    }

    fn root(&self) -> Value {
        let props = self.call("get_window_properties", json!({"windowHandle": self.window})).unwrap();
        props["rootElementHandle"].clone()
    }

    /// Handles of elements with this accessible label and role (e.g. "Button", "ListItem", "TextInput").
    pub fn find(&self, label: &str, role: &str) -> Vec<Value> {
        let query = json!([
            {"matchDescendants": true},
            {"matchElementAccessibleLabel": label},
            {"matchElementAccessibleRole": role},
        ]);
        let found = self.call("query_element_descendants", json!({"elementHandle": self.root(), "queryStack": query, "findAll": true}));
        found.map(|r| r["elementHandles"].as_array().cloned().unwrap_or_default()).unwrap_or_default()
    }

    /// Waits up to 30 s for an element to appear.
    pub fn wait_for(&self, label: &str, role: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(e) = self.find(label, role).into_iter().next() {
                return e;
            }
            if Instant::now() > deadline {
                self.screenshot("timeout");
                panic!("timed out waiting for {role} {label:?}");
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    pub fn exists(&self, label: &str, role: &str) -> bool {
        !self.find(label, role).is_empty()
    }

    pub fn click(&self, label: &str, role: &str) {
        let e = self.wait_for(label, role);
        self.call("invoke_accessibility_action", json!({"elementHandle": e, "action": "Default_"})).unwrap();
    }

    pub fn type_in(&self, label: &str, text: &str) {
        let e = self.wait_for(label, "TextInput");
        self.call("set_element_value", json!({"elementHandle": e, "value": text})).unwrap();
    }

    /// Fills and submits the setup form for a GreenMail account.
    pub fn set_up(&self, account: &AccountConfig) {
        self.type_in("Email", &account.email);
        self.type_in("Password", "x");
        self.type_in("IMAP server", "localhost");
        self.type_in("IMAP port", "3993");
        self.type_in("SMTP server", "localhost");
        self.type_in("SMTP port", "3465");
        self.click("Save and connect", "Button");
        // The mail view sits under the setup screen; it only works once the account is saved.
        self.wait_until("setup to finish", || !self.exists("Save and connect", "Button"));
    }

    /// Clicks an item of the folder pane, which is a drawer on phones.
    pub fn click_in_drawer(&self, label: &str, role: &str) {
        if android() {
            self.click("Folders", "Button");
        }
        self.click(label, role);
    }

    /// Holds a finger on the element for 0.8 s (Android only: a real touch through adb).
    pub fn long_press(&self, label: &str, role: &str) {
        assert!(android(), "long press is a touch gesture");
        let e = self.wait_for(label, role);
        let props = self.call("get_element_properties", json!({"elementHandle": e})).unwrap();
        let center = |axis: &str, extent: &str| {
            // Zero coordinates are left out of the reply.
            props["absolutePosition"][axis].as_f64().unwrap_or(0.0) + props["size"][extent].as_f64().unwrap() / 2.0
        };
        // Slint lays out in logical pixels: density / 160 physical pixels each.
        let out = adb_command().args(["shell", "wm", "density"]).output().expect("adb not found");
        let density: f64 = String::from_utf8_lossy(&out.stdout).rsplit(':').next().unwrap().trim().parse().unwrap();
        let (x, y) = ((center("x", "width") * density / 160.0) as i64, (center("y", "height") * density / 160.0) as i64);
        let (x, y) = (x.to_string(), y.to_string());
        adb(&["shell", "input", "swipe", &x, &y, &x, &y, "800"]);
    }

    /// Android's system Back key.
    pub fn back(&self) {
        assert!(android(), "Back is an Android key");
        adb(&["shell", "input", "keyevent", "KEYCODE_BACK"]);
    }

    /// Waits up to 30 s for `cond`, with a screenshot on timeout.
    pub fn wait_until(&self, what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !cond() {
            if Instant::now() > deadline {
                self.screenshot("timeout");
                panic!("timed out waiting for {what}");
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    pub fn property(&self, element: &Value, name: &str) -> Value {
        self.call("get_element_properties", json!({"elementHandle": element})).unwrap()[name].clone()
    }

    /// Saves a PNG to target/e2e/<name>.png, for failures and visual checks.
    pub fn screenshot(&self, name: &str) -> Option<PathBuf> {
        let image = self.call("take_screenshot", json!({"windowHandle": self.window})).ok()?;
        let png = base64_decode(image["data"].as_str()?);
        let path = workspace_dir().join("target/e2e").join(format!("{name}-{}.png", if android() { "android" } else { "desktop" }));
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, png).ok()?;
        eprintln!("screenshot: {}", path.display());
        Some(path)
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        } else {
            let _ = adb_command().args(["shell", "am", "force-stop", PACKAGE]).status();
        }
        if let Some(data) = &self._data {
            let _ = std::fs::remove_dir_all(data);
        }
    }
}

fn base64_decode(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let digits: Vec<u8> = s.bytes().filter(|c| *c != b'=' && !c.is_ascii_whitespace()).map(value).collect();
    digits
        .chunks(4)
        .flat_map(|c| {
            let n = c.iter().enumerate().fold(0u32, |n, (i, d)| n | (*d as u32) << (18 - 6 * i));
            n.to_be_bytes()[1..c.len()].to_vec()
        })
        .collect()
}

/// Records the notifications the app shows from now on: D-Bus on the desktop,
/// the notification service on Android.
pub struct Notifications {
    monitor: Option<Child>,
    seen: std::sync::Arc<std::sync::Mutex<String>>,
}

impl Notifications {
    pub fn watch() -> Notifications {
        let seen = std::sync::Arc::<std::sync::Mutex<String>>::default();
        if android() {
            return Notifications { monitor: None, seen };
        }
        let mut monitor = Command::new("dbus-monitor")
            .args(["--session", "interface='org.freedesktop.Notifications',member='Notify'"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("dbus-monitor not found");
        let mut out = monitor.stdout.take().unwrap();
        let sink = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = out.read(&mut buf) {
                sink.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });
        // dbus-monitor needs a moment to register with the bus.
        std::thread::sleep(Duration::from_millis(500));
        Notifications { monitor: Some(monitor), seen }
    }

    /// Whether a notification mentioning `text` was shown.
    pub fn shown(&self, text: &str) -> bool {
        if android() {
            let out = adb_command().args(["shell", "dumpsys", "notification", "--noredact"]).output().expect("adb not found");
            // --noredact prints the title and text of every posted notification.
            return String::from_utf8_lossy(&out.stdout).contains(text);
        }
        self.seen.lock().unwrap().contains(text)
    }
}

impl Drop for Notifications {
    fn drop(&mut self) {
        if let Some(monitor) = &mut self.monitor {
            let _ = monitor.kill();
            let _ = monitor.wait();
        }
    }
}

/// A fresh GreenMail account (any login is accepted, users are created on first login).
pub fn test_account() -> AccountConfig {
    // Tests run in parallel: the counter keeps their accounts apart.
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let user = format!("e2e{now}-{n}@localhost");
    AccountConfig {
        name: "E2E".into(),
        email: user.clone(),
        username: user,
        imap_host: "localhost".into(),
        imap_port: 3993,
        imap_security: Security::Tls,
        smtp_host: "localhost".into(),
        smtp_port: 3465,
        smtp_security: Security::Tls,
        auth: mailcore::Auth::Password,
    }
}

fn block_on<T>(f: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

/// Prepares the server side for tests: trusts the test CA in this process.
pub fn init_server(ca: &Path) {
    // SAFETY: called before the test starts other threads.
    unsafe { std::env::set_var("MAILCLIENT_EXTRA_CA", ca) };
    mailcore::init_tls();
}

pub fn create_folders(account: &AccountConfig, folders: &[&str]) {
    block_on(async {
        let mut imap = Imap::connect(account, "x").await.unwrap();
        for f in folders {
            imap.create(f).await.unwrap();
        }
        imap.logout().await;
    })
}

/// The messages in a server folder (empty if the folder does not exist).
pub fn server_messages(account: &AccountConfig, folder: &str) -> Vec<mailcore::MessageSummary> {
    block_on(async {
        let mut imap = Imap::connect(account, "x").await.unwrap();
        let messages = imap.fetch_new(folder, 0).await.unwrap_or_default();
        imap.logout().await;
        messages
    })
}

pub fn server_subjects(account: &AccountConfig, folder: &str) -> Vec<String> {
    server_messages(account, folder).into_iter().map(|m| m.subject).collect()
}

/// Delivers an HTML message; any HTML, also with long lines (encoded as needed).
pub fn deliver_html(account: &AccountConfig, from: (&str, &str), subject: &str, html: &str) {
    let raw = mail_builder::MessageBuilder::new()
        .from(from)
        .to(account.email.as_str())
        .subject(subject)
        .html_body(html)
        .write_to_string()
        .unwrap();
    deliver_raw(account, &raw);
}

/// Delivers a ready-made message (e.g. HTML) to the account.
pub fn deliver_raw(account: &AccountConfig, raw: &str) {
    block_on(mailcore::smtp::send(account, "x", raw.as_bytes(), std::slice::from_ref(&account.email))).unwrap();
}

/// Counts HTTP requests on a local port: stands in for a tracking pixel's server.
/// On Android the device reaches it as localhost through `adb reverse`.
pub struct PixelServer {
    pub port: u16,
    paths: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl PixelServer {
    pub fn start() -> PixelServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        if android() {
            adb(&["reverse", &format!("tcp:{port}"), &format!("tcp:{port}")]);
        }
        let paths = std::sync::Arc::<std::sync::Mutex<Vec<String>>>::default();
        let log = paths.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                // "GET /path HTTP/1.1"
                let request = String::from_utf8_lossy(&buf[..n]);
                log.lock().unwrap().push(request.split_whitespace().nth(1).unwrap_or_default().to_string());
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        PixelServer { port, paths }
    }

    /// Paths requested so far.
    pub fn requests(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

/// The device screen as RGBA rows (Android only): `adb exec-out screencap` without -p.
pub struct Screen {
    pub width: usize,
    pub height: usize,
    pixels: Vec<u8>,
}

impl Screen {
    pub fn capture() -> Screen {
        let out = adb_command().args(["exec-out", "screencap"]).output().expect("adb not found").stdout;
        let word = |i: usize| u32::from_le_bytes(out[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
        let (width, height) = (word(0), word(1));
        // The header is 12 bytes, 16 since Android 11 (color space).
        let header = out.len() - width * height * 4;
        assert!(header == 12 || header == 16, "unexpected screencap size");
        Screen { width, height, pixels: out[header..].to_vec() }
    }

    /// Whether the rectangle [x0, x1) × [y0, y1) looks different in `other`.
    pub fn differs(&self, other: &Screen, [x0, y0, x1, y1]: [usize; 4]) -> bool {
        let row = |s: &Screen, y: usize| (y * s.width + x0) * 4..(y * s.width + x1) * 4;
        (y0..y1).any(|y| self.pixels[row(self, y)] != other.pixels[row(other, y)])
    }
}

/// Screen frame [x0, y0, x1, y1] of the HTML view's window, if it is showing (Android only).
pub fn html_frame() -> Option<[usize; 4]> {
    let out = adb_command().args(["shell", "dumpsys", "window", "windows"]).output().expect("adb not found").stdout;
    let text = String::from_utf8_lossy(&out);
    let section = text.split("Window #").find(|w| w.contains("PopupWindow:") && w.contains("isVisible=true"))?;
    // "frame=[0,472][1080,2111]"
    let frame = section.split(" frame=[").nth(1)?;
    let numbers: Vec<usize> = frame
        .split(|c: char| !c.is_ascii_digit())
        .filter(|n| !n.is_empty())
        .take(4)
        .map(|n| n.parse().unwrap())
        .collect();
    numbers.try_into().ok()
}

/// A finger drag on the device, in screen pixels (Android only).
pub fn swipe((x0, y0): (usize, usize), (x1, y1): (usize, usize), ms: u32) {
    let args: Vec<String> = [x0, y0, x1, y1].iter().map(ToString::to_string).chain([ms.to_string()]).collect();
    let mut all = vec!["shell", "input", "swipe"];
    all.extend(args.iter().map(String::as_str));
    adb(&all);
}

pub fn tap(x: usize, y: usize) {
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()]);
}

/// Turns the device to landscape until dropped (Android only).
pub struct Landscape;

impl Landscape {
    pub fn turn() -> Landscape {
        adb(&["shell", "settings", "put", "system", "accelerometer_rotation", "0"]);
        adb(&["shell", "settings", "put", "system", "user_rotation", "1"]);
        Landscape
    }
}

impl Drop for Landscape {
    fn drop(&mut self) {
        let _ = adb_command().args(["shell", "settings", "put", "system", "user_rotation", "0"]).status();
    }
}

/// A screenshot of the whole device screen, native views included (Android only).
pub fn device_screenshot(name: &str) {
    let out = adb_command().args(["exec-out", "screencap", "-p"]).output().expect("adb not found");
    let path = workspace_dir().join("target/e2e").join(format!("{name}-{}.png", if android() { "android" } else { "desktop" }));
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    std::fs::write(&path, out.stdout).unwrap();
    eprintln!("screenshot: {}", path.display());
}

pub fn deliver(account: &AccountConfig, from: (&str, &str), subject: &str, body: &str) {
    let draft = Draft {
        from_name: from.0.into(),
        from_addr: from.1.into(),
        to: account.email.clone(),
        subject: subject.into(),
        body: body.into(),
        ..Default::default()
    };
    let built = mime::build_message(&draft).unwrap();
    block_on(mailcore::smtp::send(account, "x", &built.raw, &built.recipients)).unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64() {
        assert_eq!(super::base64_decode("aGVsbG8="), b"hello");
        assert_eq!(super::base64_decode("aGk="), b"hi");
        assert_eq!(super::base64_decode("YWJj"), b"abc");
    }
}
