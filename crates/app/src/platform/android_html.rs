//! The HTML message view on Android: java/dev/mailclient/MailWebView.java, a WebView over the
//! reader area with the same request policy as the desktop's Servo view.

use std::collections::HashMap;
use std::sync::OnceLock;

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::{JNIEnv, NativeMethod};

use super::android::{Env, Obj, activity, load_class, with_context};

static CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// The conversation to show: one document per message, stacked; "" while a body is loading.
pub struct Message<'a> {
    pub documents: &'a [String],
    /// Sender and date per message; empty for a lone message.
    pub headings: &'a [String],
    /// The open message, scrolled to.
    pub current: usize,
    pub csp: &'a str,
    pub allow_remote: bool,
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
        let documents = strings(env, message.documents)?;
        let headings = strings(env, message.headings)?;
        let csp = env.new_string(message.csp)?;
        let class = class(env, activity)?;
        env.call_static_method(
            class,
            "show",
            "(Landroid/app/Activity;[Ljava/lang/String;[Ljava/lang/String;ILjava/lang/String;Z[Ljava/lang/String;[Ljava/lang/String;[[B)V",
            &[
                activity.into(),
                (&documents).into(),
                (&headings).into(),
                (message.current as i32).into(),
                (&csp).into(),
                message.allow_remote.into(),
                (&cids).into(),
                (&mimes).into(),
                (&data).into(),
            ],
        )?;
        Ok(())
    });
}

fn strings<'e>(env: &mut Env<'e>, items: &[String]) -> jni::errors::Result<jni::objects::JObjectArray<'e>> {
    let array = env.new_object_array(items.len() as i32, "java/lang/String", JObject::null())?;
    for (i, item) in items.iter().enumerate() {
        let item = env.new_string(item)?;
        env.set_object_array_element(&array, i as i32, item)?;
    }
    Ok(array)
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
    let on_scrolled_to: extern "system" fn(JNIEnv, JClass, jni::sys::jint) = on_scrolled_to;
    env.register_native_methods(
        &class,
        &[
            NativeMethod { name: "onLink".into(), sig: "(Ljava/lang/String;)V".into(), fn_ptr: on_link as *mut _ },
            NativeMethod { name: "onBack".into(), sig: "()V".into(), fn_ptr: on_back as *mut _ },
            NativeMethod { name: "onScrolledTo".into(), sig: "(I)V".into(), fn_ptr: on_scrolled_to as *mut _ },
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

/// Called by MailWebView on the Android UI thread: the user scrolled the conversation to this message.
extern "system" fn on_scrolled_to(_env: JNIEnv, _class: JClass, index: jni::sys::jint) {
    crate::app::scrolled_to_later(index);
}
