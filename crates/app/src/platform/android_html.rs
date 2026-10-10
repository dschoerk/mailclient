//! The HTML message view on Android: java/dev/mailclient/MailWebView.java, a WebView over the
//! reader area with the same request policy as the desktop's Servo view.

use std::collections::HashMap;
use std::sync::OnceLock;

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::{JNIEnv, NativeMethod};

use super::android::{Env, Obj, activity, load_class, with_context};

static CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// The message to show, as the desktop view gets it (`webview::delegate::MessageContent`).
pub struct Message<'a> {
    pub document: &'a str,
    pub csp: &'a str,
    pub allow_remote: bool,
    pub anchor: &'a str,
    /// Content-ID -> (mime type, bytes).
    pub inline_parts: &'a HashMap<String, (String, Vec<u8>)>,
}

pub fn show(message: &Message) {
    call("show", |env, activity| {
        let cids = env.new_object_array(message.inline_parts.len() as i32, "java/lang/String", JObject::null())?;
        let mimes = env.new_object_array(message.inline_parts.len() as i32, "java/lang/String", JObject::null())?;
        let data = env.new_object_array(message.inline_parts.len() as i32, "[B", JObject::null())?;
        for (i, (cid, (mime, bytes))) in message.inline_parts.iter().enumerate() {
            let (cid, mime, bytes) = (env.new_string(cid)?, env.new_string(mime)?, env.byte_array_from_slice(bytes)?);
            env.set_object_array_element(&cids, i as i32, cid)?;
            env.set_object_array_element(&mimes, i as i32, mime)?;
            env.set_object_array_element(&data, i as i32, bytes)?;
        }
        let document = env.new_string(message.document)?;
        let csp = env.new_string(message.csp)?;
        let anchor = env.new_string(message.anchor)?;
        let class = class(env, activity)?;
        env.call_static_method(
            class,
            "show",
            "(Landroid/app/Activity;Ljava/lang/String;Ljava/lang/String;ZLjava/lang/String;[Ljava/lang/String;[Ljava/lang/String;[[B)V",
            &[
                activity.into(),
                (&document).into(),
                (&csp).into(),
                message.allow_remote.into(),
                (&anchor).into(),
                (&cids).into(),
                (&mimes).into(),
                (&data).into(),
            ],
        )?;
        Ok(())
    });
}

pub fn clear() {
    call("clear", |env, activity| {
        let class = class(env, activity)?;
        env.call_static_method(class, "clear", "(Landroid/app/Activity;)V", &[activity.into()])?;
        Ok(())
    });
}

/// Moves the view over the reader area: physical pixels from the window's top left.
pub fn place(x: i32, y: i32, width: i32, height: i32, visible: bool) {
    call("place", |env, activity| {
        let class = class(env, activity)?;
        let args: [JValue; 6] = [activity.into(), x.into(), y.into(), width.into(), height.into(), visible.into()];
        env.call_static_method(class, "place", "(Landroid/app/Activity;IIIIZ)V", &args)?;
        Ok(())
    });
}

/// The native floating buttons: `side` 0 none, 1 right, 2 left; colors (ARGB) background, pressed, foreground, border.
pub fn buttons(side: i32, colors: [i32; 4]) {
    call("buttons", |env, activity| {
        let class = class(env, activity)?;
        let [bg, pressed, fg, border] = colors;
        let args: [JValue; 6] = [activity.into(), side.into(), bg.into(), pressed.into(), fg.into(), border.into()];
        env.call_static_method(class, "buttons", "(Landroid/app/Activity;IIIII)V", &args)?;
        Ok(())
    });
}

fn call(what: &str, f: impl FnOnce(&mut Env, &Obj) -> jni::errors::Result<()>) {
    let Some(activity) = activity() else { return };
    if let Err(e) = with_context(|env, _| f(env, activity.as_obj())) {
        log::warn!("HTML view ({what}): {e}");
    }
}

/// MailWebView, loaded on first use.
fn class(env: &mut Env, activity: &Obj) -> jni::errors::Result<&'static JClass<'static>> {
    if let Some(class) = CLASS.get() {
        return Ok(<&JClass>::from(class.as_obj()));
    }
    let class = load_class(env, activity, "dev.mailclient.MailWebView")?;
    let on_link: extern "system" fn(JNIEnv, JClass, JString) = on_link;
    let on_back: extern "system" fn(JNIEnv, JClass) = on_back;
    let on_float: extern "system" fn(JNIEnv, JClass, i32) = on_float;
    env.register_native_methods(
        &class,
        &[
            NativeMethod { name: "onLink".into(), sig: "(Ljava/lang/String;)V".into(), fn_ptr: on_link as *mut _ },
            NativeMethod { name: "onBack".into(), sig: "()V".into(), fn_ptr: on_back as *mut _ },
            NativeMethod { name: "onFloat".into(), sig: "(I)V".into(), fn_ptr: on_float as *mut _ },
        ],
    )?;
    let global = env.new_global_ref(class)?;
    Ok(<&JClass>::from(CLASS.get_or_init(|| global).as_obj()))
}

/// Called by MailWebView on the Android UI thread.
extern "system" fn on_link(mut env: JNIEnv, _class: JClass, url: JString) {
    let url: String = env.get_string(&url).map(Into::into).unwrap_or_default();
    crate::app::open_link_later(url);
}

/// Called by MailWebView on the Android UI thread: the view has key focus, so Back arrives here.
extern "system" fn on_back(_env: JNIEnv, _class: JClass) {
    crate::app::back_later();
}

/// Called by MailWebView on the Android UI thread: a floating button was tapped.
extern "system" fn on_float(_env: JNIEnv, _class: JClass, action: i32) {
    crate::app::float_action_later(action);
}
