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
    // ndk_context holds the Application; asking for a permission needs the Activity itself.
    let activity = android_app.activity_as_ptr();
    slint::android::init(android_app).expect("Slint Android backend");
    // SAFETY: the activity outlives android_main, which returns when it is destroyed.
    let keep = |env: &mut Env, _: &Obj| env.new_global_ref(unsafe { Obj::from_raw(activity.cast()) });
    match with_context(keep) {
        Ok(activity) => drop(ACTIVITY.set(activity)),
        Err(e) => log::warn!("cannot reference the activity: {e}"),
    }
    let request = |env: &mut Env, _: &Obj| match self::activity() {
        Some(activity) => request_notification_permission(env, activity.as_obj()),
        None => Ok(()),
    };
    if let Err(e) = with_context(request) {
        log::warn!("cannot ask for the notification permission: {e}");
    }
    if let Err(e) = crate::run() {
        log::error!("{e:#}");
    }
}

pub fn init_logging() {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("mailclient"));
}

/// Opens `url` with an ACTION_VIEW intent, which Android routes to the default browser.
pub fn open_url(url: &str) -> Result<()> {
    with_context(|env, context| start_view(env, context, url)).map_err(|e| anyhow!("opening {url}: {e}"))
}

/// Posts a new-mail notification; tapping it opens the app. A newer one replaces the older.
pub fn notify(title: &str, body: &str) {
    if let Err(e) = with_context(|env, context| post_notification(env, context, title, body)) {
        log::warn!("cannot show notification: {e}");
    }
}

/// Android's FLAG_ACTIVITY_NEW_TASK: needed when starting an activity from a context that is not one.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
const PENDING_INTENT_FLAG_IMMUTABLE: i32 = 0x0400_0000;
const NOTIFICATION_IMPORTANCE_DEFAULT: i32 = 3;
const PERMISSION_GRANTED: i32 = 0;
const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";

pub(super) type Env<'a> = jni::JNIEnv<'a>;
pub(super) type Obj<'a> = jni::objects::JObject<'a>;

static ACTIVITY: std::sync::OnceLock<jni::objects::GlobalRef> = std::sync::OnceLock::new();

/// The running Activity; ndk_context only has the Application, which cannot show views.
pub(super) fn activity() -> Option<&'static jni::objects::GlobalRef> {
    ACTIVITY.get()
}

/// Runs `f` with the JNI environment and the application context.
pub(super) fn with_context<T>(f: impl FnOnce(&mut Env, &Obj) -> jni::errors::Result<T>) -> jni::errors::Result<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity fills ndk_context with the process's JavaVM and the application context.
    let (vm, context) = unsafe {
        (jni::JavaVM::from_raw(ctx.vm().cast())?, jni::objects::JObject::from_raw(ctx.context().cast()))
    };
    let mut env = vm.attach_current_thread()?;
    let result = f(&mut env, &context);
    // A Java exception left pending kills the process when the thread detaches; report it as an error instead.
    if env.exception_check()? {
        env.exception_describe()?;
        env.exception_clear()?;
    }
    result
}

fn start_view(env: &mut jni::JNIEnv, context: &jni::objects::JObject, url: &str) -> jni::errors::Result<()> {
    let url = env.new_string(url)?;
    let uri = env
        .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[(&url).into()])?
        .l()?;
    let action = env.new_string("android.intent.action.VIEW")?;
    let intent =
        env.new_object("android/content/Intent", "(Ljava/lang/String;Landroid/net/Uri;)V", &[(&action).into(), (&uri).into()])?;
    env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[FLAG_ACTIVITY_NEW_TASK.into()])?;
    env.call_method(context, "startActivity", "(Landroid/content/Intent;)V", &[(&intent).into()])?;
    Ok(())
}

/// Android 13+ shows notifications only with this runtime permission; asks once, on first start.
fn request_notification_permission(env: &mut Env, activity: &Obj) -> jni::errors::Result<()> {
    let permission = env.new_string(POST_NOTIFICATIONS)?;
    let granted = env.call_method(activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[(&permission).into()])?.i()?;
    if granted == PERMISSION_GRANTED {
        return Ok(());
    }
    let list = env.new_object_array(1, "java/lang/String", &permission)?;
    env.call_method(activity, "requestPermissions", "([Ljava/lang/String;I)V", &[(&list).into(), 1.into()])?;
    Ok(())
}

fn post_notification(env: &mut Env, context: &Obj, title: &str, body: &str) -> jni::errors::Result<()> {
    let service = env.new_string("notification")?;
    let manager =
        env.call_method(context, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[(&service).into()])?.l()?;
    // Creating a channel that exists already does nothing.
    let channel_id = env.new_string("new-mail")?;
    let channel_name = env.new_string("New mail")?;
    let channel = env.new_object(
        "android/app/NotificationChannel",
        "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
        &[(&channel_id).into(), (&channel_name).into(), NOTIFICATION_IMPORTANCE_DEFAULT.into()],
    )?;
    env.call_method(&manager, "createNotificationChannel", "(Landroid/app/NotificationChannel;)V", &[(&channel).into()])?;

    let package = env.call_method(context, "getPackageName", "()Ljava/lang/String;", &[])?.l()?;
    let packages = env.call_method(context, "getPackageManager", "()Landroid/content/pm/PackageManager;", &[])?.l()?;
    let launch = env
        .call_method(&packages, "getLaunchIntentForPackage", "(Ljava/lang/String;)Landroid/content/Intent;", &[(&package).into()])?
        .l()?;
    let on_tap = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[context.into(), 0.into(), (&launch).into(), PENDING_INTENT_FLAG_IMMUTABLE.into()],
        )?
        .l()?;
    // shortcut: the system mail icon; the app has no icon resources of its own yet.
    let icon = env.get_static_field("android/R$drawable", "ic_dialog_email", "I")?.i()?;
    let title = env.new_string(title)?;
    let body = env.new_string(body)?;
    let builder = env.new_object(
        "android/app/Notification$Builder",
        "(Landroid/content/Context;Ljava/lang/String;)V",
        &[context.into(), (&channel_id).into()],
    )?;
    const B: &str = "Landroid/app/Notification$Builder;";
    env.call_method(&builder, "setSmallIcon", format!("(I){B}"), &[icon.into()])?;
    env.call_method(&builder, "setContentTitle", format!("(Ljava/lang/CharSequence;){B}"), &[(&title).into()])?;
    env.call_method(&builder, "setContentText", format!("(Ljava/lang/CharSequence;){B}"), &[(&body).into()])?;
    env.call_method(&builder, "setContentIntent", format!("(Landroid/app/PendingIntent;){B}"), &[(&on_tap).into()])?;
    env.call_method(&builder, "setAutoCancel", format!("(Z){B}"), &[true.into()])?;
    let notification = env.call_method(&builder, "build", "()Landroid/app/Notification;", &[])?.l()?;
    env.call_method(&manager, "notify", "(ILandroid/app/Notification;)V", &[1.into(), (&notification).into()])?;
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
