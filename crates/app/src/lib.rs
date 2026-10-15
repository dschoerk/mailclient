mod app;
mod compose;
mod format;
mod platform;
#[cfg(test)]
mod story_tests;
#[cfg(feature = "servo")]
mod webview;

#[cfg(feature = "servo")]
mod gl_bindings {
    #![allow(unsafe_op_in_unsafe_fn, clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

slint::include_modules!();

pub fn run() -> anyhow::Result<()> {
    platform::init_logging();
    mailcore::init_tls();

    #[cfg(feature = "servo")]
    slint::BackendSelector::new()
        .require_wgpu_30(slint::wgpu_30::WGPUConfiguration::Automatic(slint::wgpu_30::WGPUSettings::default()))
        .select()
        .map_err(|e| anyhow::anyhow!("no wgpu-capable graphics backend: {e}"))?;

    let (config, eml) = parse_args(std::env::args_os().skip(1))?;
    if let Some(path) = config {
        mailcore::config::set_config_file(path);
    }
    let window = AppWindow::new()?;
    app::Controller::install(&window, eml)?;
    // Started by tapping a notification: MainActivity.onResume came before the Rust side was up.
    #[cfg(target_os = "android")]
    platform::open_notified();
    // The first frame shows the inbox from the cache: Android's "Fully drawn" time, which
    // scripts/measure-android.sh reads (its own launch time ends at the splash screen).
    #[cfg(target_os = "android")]
    {
        let reported = std::cell::Cell::new(false);
        let notified = window.window().set_rendering_notifier(move |state, _| {
            if matches!(state, slint::RenderingState::AfterRendering) && !reported.replace(true) {
                platform::fully_drawn();
            }
        });
        if let Err(e) = notified {
            log::warn!("no startup time: {e}");
        }
    }
    #[cfg(feature = "servo")]
    if let Some(path) = std::env::var_os("MAILCLIENT_SNAPSHOT") {
        schedule_snapshot(&window, path.into());
    }
    window.run()?;
    app::Controller::shutdown();
    Ok(())
}

/// `mailclient [--config <file.toml>] [message.eml]` → (config file, message file).
fn parse_args(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> anyhow::Result<(Option<std::path::PathBuf>, Option<std::path::PathBuf>)> {
    let (mut config, mut eml) = (None, None);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            config = Some(args.next().ok_or_else(|| anyhow::anyhow!("--config needs a file"))?.into());
        } else {
            eml = Some(arg.into());
        }
    }
    Ok((config, eml))
}

#[cfg(test)]
#[test]
fn parses_args() {
    let parse = |args: &[&str]| parse_args(args.iter().map(Into::into));
    let some = |p: &str| Some(std::path::PathBuf::from(p));
    assert_eq!(parse(&[]).unwrap(), (None, None));
    assert_eq!(parse(&["m.eml"]).unwrap(), (None, some("m.eml")));
    assert_eq!(parse(&["--config", "c.toml", "m.eml"]).unwrap(), (some("c.toml"), some("m.eml")));
    assert!(parse(&["--config"]).is_err());
}

/// The hidden webview keeps its last rendered page; it must not squash the reply body.
#[cfg(test)]
#[test]
fn reply_body_is_not_squashed_by_hidden_webview() {
    use slint::ComponentHandle;
    i_slint_backend_testing::init_no_event_loop();
    let w = AppWindow::new().unwrap();
    w.window().set_size(slint::PhysicalSize::new(1200, 800));
    w.set_use_webview(true);
    // A rendered message taller than the window.
    let page = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(600, 2000);
    w.global::<WebviewLogic>().set_web_content(slint::Image::from_rgba8(page));
    w.show().unwrap();
    w.set_has_message(true);
    w.set_compose_to("someone@example.org".into());
    w.set_composing(true);
    let body = i_slint_backend_testing::ElementHandle::find_by_accessible_label(&w, "Body").next().expect("Body field");
    assert!(body.size().height > 300.0, "reply body squashed to {}px", body.size().height);
}

/// The tools row offers the message tools; the source view replaces the body with the raw message and closes again.
#[cfg(test)]
#[test]
fn shows_message_source() {
    use slint::ComponentHandle;
    i_slint_backend_testing::init_no_event_loop();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.eml");
    std::fs::write(&path, "From: a@example.org\r\nSubject: Source test\r\nX-Probe: 42\r\n\r\nHello\r\n").unwrap();
    let w = AppWindow::new().unwrap();
    app::Controller::install(&w, Some(path)).unwrap();
    w.show().unwrap();
    let find = |label: &str| i_slint_backend_testing::ElementHandle::find_by_accessible_label(&w, label).next();
    let source = || find("Message source");
    assert!(source().is_none());
    // The tools row opens from the toolbar.
    assert!(find("Print").is_none());
    find("More tools").expect("More tools button").invoke_accessible_default_action();
    for tool in ["View source", "Save as .eml", "Save as PDF", "Print"] {
        assert!(find(tool).is_some(), "{tool} in the tools row");
    }
    w.invoke_view_source();
    assert!(w.get_source_open());
    assert!(w.get_msg_source().contains("X-Probe: 42"), "{}", w.get_msg_source());
    assert!(source().is_some(), "source view shown");
    w.invoke_view_source();
    assert!(!w.get_source_open());
    assert!(source().is_none());
    app::Controller::shutdown();
}

/// Debugging aid: saves a picture of the window a few seconds after start and exits.
#[cfg(feature = "servo")]
fn schedule_snapshot(window: &AppWindow, path: std::path::PathBuf) {
    use slint::ComponentHandle;
    let weak = window.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_secs(6), move || {
        let Some(window) = weak.upgrade() else { return };
        match window.window().take_snapshot() {
            Ok(buffer) => {
                let image = image::RgbaImage::from_raw(buffer.width(), buffer.height(), buffer.as_bytes().to_vec());
                match image.map(|i| i.save(&path)) {
                    Some(Ok(())) => log::info!("snapshot saved to {}", path.display()),
                    other => log::error!("saving snapshot failed: {other:?}"),
                }
            }
            Err(e) => log::error!("snapshot failed: {e}"),
        }
        let _ = slint::quit_event_loop();
    });
}
