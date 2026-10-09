use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};

#[unsafe(no_mangle)]
fn android_main(android_app: slint::android::AndroidApp) {
    init_logging();
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
    if let Err(e) = crate::run() {
        log::error!("{e:#}");
    }
}

pub fn init_logging() {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("mailclient"));
}

/// Opens `url` with an ACTION_VIEW intent, which Android routes to the default browser.
pub fn open_url(url: &str) -> Result<()> {
    android_view(url).map_err(|e| anyhow!("opening {url}: {e}"))
}

/// Android's FLAG_ACTIVITY_NEW_TASK: needed when starting an activity from a context that is not one.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;

fn android_view(url: &str) -> jni::errors::Result<()> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity fills ndk_context with the process's JavaVM and the running activity.
    let (vm, activity) = unsafe {
        (jni::JavaVM::from_raw(ctx.vm().cast())?, jni::objects::JObject::from_raw(ctx.context().cast()))
    };
    let mut env = vm.attach_current_thread()?;
    let result = start_view(&mut env, &activity, url);
    // A Java exception left pending kills the process when the thread detaches; report it as an error instead.
    if env.exception_check()? {
        env.exception_describe()?;
        env.exception_clear()?;
    }
    result
}

fn start_view(env: &mut jni::JNIEnv, activity: &jni::objects::JObject, url: &str) -> jni::errors::Result<()> {
    let url = env.new_string(url)?;
    let uri = env
        .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[(&url).into()])?
        .l()?;
    let action = env.new_string("android.intent.action.VIEW")?;
    let intent =
        env.new_object("android/content/Intent", "(Ljava/lang/String;Landroid/net/Uri;)V", &[(&action).into(), (&uri).into()])?;
    env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[FLAG_ACTIVITY_NEW_TASK.into()])?;
    env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[(&intent).into()])?;
    Ok(())
}

/// shortcut: no file picker on Android yet (needs the Storage Access Framework via JNI).
pub async fn pick_files() -> Result<Vec<PathBuf>> {
    bail!("Attaching files is not supported on Android yet")
}

/// shortcut: no file picker on Android yet (needs the Storage Access Framework via JNI).
pub async fn save_file(_name: &str) -> Result<Option<PathBuf>> {
    bail!("Saving attachments is not supported on Android yet")
}
