use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};
use jni::objects::{JClass, JObject, JObjectArray, JString};
use jni::{JNIEnv, NativeMethod};
use tokio::sync::oneshot;

#[unsafe(no_mangle)]
fn android_main(android_app: slint::android::AndroidApp) {
    init_logging();
    if let Some(dir) = android_app.internal_data_path() {
        // End-to-end tests: port for Slint's MCP server (only in builds with `slint/mcp`).
        if let Ok(port) = std::fs::read_to_string(dir.join("mcp-port")) {
            // SAFETY: no other threads exist yet.
            unsafe { std::env::set_var("SLINT_MCP_PORT", port.trim()) };
        }
        use_data_dir(dir);
    }
    // ndk_context holds the Application; asking for a permission needs the Activity itself.
    let activity = android_app.activity_as_ptr();
    slint::android::init(android_app).expect("Slint Android backend");
    // SAFETY: the activity outlives android_main, which returns when it is destroyed.
    let keep = |env: &mut Env, _: &Obj| env.new_global_ref(unsafe { Obj::from_raw(activity.cast()) });
    match with_context(keep) {
        Ok(activity) => *ACTIVITY.lock().unwrap() = Some(activity),
        Err(e) => log::warn!("cannot reference the activity: {e}"),
    }
    let request = |env: &mut Env, _: &Obj| match self::activity() {
        Some(activity) => request_notification_permission(env, activity.as_obj()),
        None => Ok(()),
    };
    if let Err(e) = with_context(request) {
        log::warn!("cannot ask for the notification permission: {e}");
    }
    if let Err(e) = on_activity(|env, activity| {
        let on_result: extern "system" fn(JNIEnv, JClass, JObjectArray, JString) = on_result;
        let on_resumed: extern "system" fn(JNIEnv, JClass) = on_resumed;
        let class = env.get_object_class(activity)?;
        let methods = [
            NativeMethod {
                name: "onResult".into(),
                sig: "([Ljava/lang/String;Ljava/lang/String;)V".into(),
                fn_ptr: on_result as *mut _,
            },
            NativeMethod { name: "onResumed".into(), sig: "()V".into(), fn_ptr: on_resumed as *mut _ },
        ];
        env.register_native_methods(&class, &methods)
    }) {
        log::warn!("cannot register the activity callbacks: {e}");
    }
    if let Err(e) = crate::run() {
        log::error!("{e:#}");
    }
}

/// No home directory on Android: config and cache live in the app's private storage.
fn use_data_dir(dir: PathBuf) {
    // Development: no environment on Android, so MAILCLIENT_EXTRA_CA comes from a file
    // (copied in with `adb shell run-as`, which only works for debug builds).
    let ca = dir.join("extra-ca.pem");
    if ca.exists() {
        // SAFETY: called before any other thread of ours starts.
        unsafe { std::env::set_var("MAILCLIENT_EXTRA_CA", ca) };
    }
    mailcore::config::set_base_dir(dir);
}

/// Called by MainActivity.onResume. In the background Android freezes the process and IDLE
/// with it, so mail that arrived meanwhile is fetched now.
extern "system" fn on_resumed(_env: JNIEnv, _class: JClass) {
    crate::app::sync_now();
    open_notified();
}

/// Opens the message of the notification that started or brought back the app, if one did.
pub fn open_notified() {
    let taken = on_activity(|env, activity| {
        let open = env.call_method(activity, "takeOpen", "()Ljava/lang/String;", &[])?.l()?;
        if open.is_null() { Ok(None) } else { Ok(Some(env.get_string(&JString::from(open))?.into())) }
    });
    match taken {
        Ok(Some(open)) => crate::app::open_notified_later(open),
        Ok(None) => {}
        Err(e) => log::warn!("cannot read the tapped notification: {e}"),
    }
}

/// SyncJob's periodic check, in its own process (`:sync`) without the UI: syncs each inbox
/// once and notifies about new mail. `files_dir` is Context.getFilesDir().
#[unsafe(no_mangle)]
extern "system" fn Java_dev_mailclient_SyncJob_checkMail(mut env: JNIEnv, _class: JClass, context: JObject, files_dir: JString) {
    init_logging();
    // The keystore and notifications reach Java through ndk_context, which only the activity
    // sets up otherwise; this process never runs one, so setting it here cannot clash.
    static CONTEXT: std::sync::OnceLock<Option<jni::objects::GlobalRef>> = std::sync::OnceLock::new();
    let context = CONTEXT.get_or_init(|| {
        let global = env.new_global_ref(&context).ok()?;
        let vm = env.get_java_vm().ok()?;
        // SAFETY: the global reference is kept for the life of the process.
        unsafe { ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), global.as_obj().as_raw().cast()) };
        let dir: String = env.get_string(&files_dir).ok()?.into();
        use_data_dir(PathBuf::from(dir));
        mailcore::init_tls();
        Some(global)
    });
    if context.is_none() {
        return log::warn!("background check: no Android context");
    }
    if let Err(e) = check_mail() {
        log::warn!("background check: {e:#}");
    }
}

fn check_mail() -> Result<()> {
    if mailcore::config::load_settings()?.mute_notifications {
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    for account in mailcore::config::load_accounts()? {
        match runtime.block_on(mailcore::engine::check_inbox(&account, &crate::app::db_path(&account)?)) {
            Ok(arrived) => {
                log::info!("background check of {}: {} new", account.email, arrived.len());
                if !arrived.is_empty() {
                    crate::app::notify_new_mail(&account.email, &arrived);
                }
            }
            Err(e) => log::warn!("background check of {}: {e:#}", account.email),
        }
    }
    Ok(())
}

pub fn init_logging() {
    android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("mailclient"));
}

/// Opens `url` with an ACTION_VIEW intent, which Android routes to the default browser.
pub fn open_url(url: &str) -> Result<()> {
    with_context(|env, context| start_view(env, context, url)).map_err(|e| anyhow!("opening {url}: {e}"))
}

/// Tells Android the app's first screen is drawn (Activity.reportFullyDrawn).
pub fn fully_drawn() {
    if let Err(e) = on_activity(|env, activity| env.call_method(activity, "reportFullyDrawn", "()V", &[]).map(drop)) {
        log::warn!("cannot report the first frame: {e}");
    }
}

/// The system's long-press vibration, if the user has haptics on.
pub fn haptic() {
    if let Err(e) = on_activity(|env, activity| env.call_method(activity, "haptic", "()V", &[]).map(drop)) {
        log::warn!("cannot vibrate: {e}");
    }
}

/// Posts a new-mail notification; tapping it opens the app, which then opens `open`
/// (see MainActivity.takeOpen). A newer one replaces the older.
pub fn notify(title: &str, body: &str, open: &str) {
    if let Err(e) = with_context(|env, context| post_notification(env, context, title, body, open)) {
        log::warn!("cannot show notification: {e}");
    }
}

/// Android's FLAG_ACTIVITY_NEW_TASK: needed when starting an activity from a context that is not one.
const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
const PENDING_INTENT_FLAG_IMMUTABLE: i32 = 0x0400_0000;
/// The new notification's extras replace those of the one it replaces.
const PENDING_INTENT_FLAG_UPDATE_CURRENT: i32 = 0x0800_0000;
const NOTIFICATION_IMPORTANCE_DEFAULT: i32 = 3;
const PERMISSION_GRANTED: i32 = 0;
const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";

pub(super) type Env<'a> = jni::JNIEnv<'a>;
pub(super) type Obj<'a> = jni::objects::JObject<'a>;

/// Replaced on each android_main: a destroyed activity's successor can run android_main
/// again in the same process.
static ACTIVITY: Mutex<Option<jni::objects::GlobalRef>> = Mutex::new(None);

/// The running Activity; ndk_context only has the Application, which cannot show views.
pub(super) fn activity() -> Option<jni::objects::GlobalRef> {
    ACTIVITY.lock().unwrap().clone()
}

/// Runs `f` with the JNI environment and the activity (dev.mailclient.MainActivity).
fn on_activity<T>(f: impl FnOnce(&mut Env, &Obj) -> jni::errors::Result<T>) -> jni::errors::Result<T> {
    let activity = activity().ok_or(jni::errors::Error::NullPtr("activity"))?;
    with_context(|env, _| f(env, activity.as_obj()))
}

/// A class of the app's own Java code (crates/app/java/); FindClass would not see it from a native thread.
pub(super) fn load_class<'a>(env: &mut Env<'a>, activity: &Obj, name: &str) -> jni::errors::Result<JClass<'a>> {
    let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string(name)?;
    Ok(env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[(&name).into()])?.l()?.into())
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

fn post_notification(env: &mut Env, context: &Obj, title: &str, body: &str, open: &str) -> jni::errors::Result<()> {
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
    let (key, open) = (env.new_string("open")?, env.new_string(open)?);
    env.call_method(&launch, "putExtra", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;", &[(&key).into(), (&open).into()])?;
    let on_tap = env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[context.into(), 0.into(), (&launch).into(), (PENDING_INTENT_FLAG_IMMUTABLE | PENDING_INTENT_FLAG_UPDATE_CURRENT).into()],
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

/// The picker's answer from MainActivity.onResult: values, or None when cancelled.
type Answer = std::result::Result<Option<Vec<String>>, String>;

/// The open picker's caller. Opening another one drops it, which reads as cancelled.
static WAITING: Mutex<Option<oneshot::Sender<Answer>>> = Mutex::new(None);

/// Opens a system picker with `open` and waits for MainActivity to report back.
async fn picker(open: impl FnOnce(&mut Env, &Obj) -> jni::errors::Result<()>) -> Result<Option<Vec<String>>> {
    let (tx, rx) = oneshot::channel();
    *WAITING.lock().unwrap() = Some(tx);
    on_activity(open)?;
    rx.await.unwrap_or(Ok(None)).map_err(|e| anyhow!(e))
}

/// Called by MainActivity, on a thread of its own.
extern "system" fn on_result(mut env: JNIEnv, _class: JClass, values: JObjectArray, error: JString) {
    let string = |env: &mut JNIEnv, s: &JString| env.get_string(s).map(String::from).unwrap_or_default();
    let answer = if !error.is_null() {
        Err(string(&mut env, &error))
    } else if values.is_null() {
        Ok(None)
    } else {
        let mut strings = Vec::new();
        for i in 0..env.get_array_length(&values).unwrap_or(0) {
            if let Ok(s) = env.get_object_array_element(&values, i) {
                strings.push(string(&mut env, &JString::from(s)));
            }
        }
        Ok(Some(strings))
    };
    if let Some(tx) = WAITING.lock().unwrap().take() {
        let _ = tx.send(answer);
    }
}

/// Files the user picked to attach, copied into the app's cache; empty when cancelled.
pub async fn pick_files() -> Result<Vec<PathBuf>> {
    let paths = picker(|env, activity| env.call_method(activity, "pickFiles", "()V", &[]).map(drop)).await?;
    Ok(paths.unwrap_or_default().into_iter().map(PathBuf::from).collect())
}

/// Saves `data` as `name` in Downloads (before Android 10: where the user chooses); where it went, or `None` when cancelled.
pub async fn save_file(name: &str, mime_type: &str, data: Vec<u8>) -> Result<Option<String>> {
    let saved = picker(|env, activity| {
        let (name, mime_type, data) = (env.new_string(name)?, env.new_string(mime_type)?, env.byte_array_from_slice(&data)?);
        let args = [(&name).into(), (&mime_type).into(), (&data).into()];
        env.call_method(activity, "saveFile", "(Ljava/lang/String;Ljava/lang/String;[B)V", &args).map(drop)
    })
    .await?;
    Ok(saved.and_then(|names| names.into_iter().next()))
}
