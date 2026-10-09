mod app;
mod compose;
mod format;
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

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(android_app: slint::android::AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("mailclient"));
    // No home directory on Android: keep config and cache in the app's private storage.
    if let Some(dir) = android_app.internal_data_path() {
        // Development: no environment on Android, so MAILCLIENT_EXTRA_CA comes from a file
        // (copied in with `adb shell run-as`, which only works for debug builds).
        let ca = dir.join("extra-ca.pem");
        if ca.exists() {
            // SAFETY: no other threads exist yet.
            unsafe { std::env::set_var("MAILCLIENT_EXTRA_CA", ca) };
        }
        // End-to-end tests: port for Slint's MCP server (only in builds with `slint/mcp`).
        if let Ok(port) = std::fs::read_to_string(dir.join("mcp-port")) {
            // SAFETY: no other threads exist yet.
            unsafe { std::env::set_var("SLINT_MCP_PORT", port.trim()) };
        }
        mailcore::config::set_base_dir(dir);
    }
    slint::android::init(android_app).expect("Slint Android backend");
    if let Err(e) = run() {
        log::error!("{e:#}");
    }
}

pub fn run() -> anyhow::Result<()> {
    #[cfg(not(target_os = "android"))]
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn,mailclient_app=info,mailcore=info"))
        .init();
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
