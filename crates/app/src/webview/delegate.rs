// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT
// Adapted from Slint's examples/servo: adds the mail security policy (request
// blocking, navigation interception) on top of frame forwarding.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use http::{HeaderMap, HeaderValue, StatusCode, header};
use servo::{
    CreateNewWebViewRequest, Cursor, LoadStatus, NavigationRequest, WebResourceLoad, WebResourceResponse, WebView,
    WebViewDelegate,
};
use slint::ComponentHandle;
use url::Url;

use crate::{AppWindow, WebviewLogic, webview::SlintServoAdapter};
use mailcore::mime::CONTENT_ORIGIN;

/// The message currently shown, served to Servo from memory.
#[derive(Default)]
pub struct MessageContent {
    pub document: String,
    pub csp: String,
    /// Content-ID -> (mime type, bytes).
    pub inline_parts: HashMap<String, (String, Vec<u8>)>,
    pub allow_remote: bool,
}

/// Holds back loads until Servo has finished creating the browsing context;
/// loads issued earlier are silently dropped by Servo.
#[derive(Default)]
pub struct LoadGate {
    pub ready: std::cell::Cell<bool>,
    pub pending: RefCell<Option<Url>>,
}

pub struct Hooks {
    /// A link was activated. Navigation inside the view is always denied.
    pub on_link: Box<dyn Fn(Url)>,
    /// Hovered link target, shown so users can see where a link really goes.
    pub on_status: Box<dyn Fn(Option<String>)>,
}

pub struct AppDelegate {
    app: slint::Weak<AppWindow>,
    adapter: Rc<SlintServoAdapter>,
    content: Rc<RefCell<MessageContent>>,
    gate: Rc<LoadGate>,
    hooks: Hooks,
}

impl AppDelegate {
    pub fn new(
        app: &AppWindow,
        adapter: Rc<SlintServoAdapter>,
        content: Rc<RefCell<MessageContent>>,
        gate: Rc<LoadGate>,
        hooks: Hooks,
    ) -> Self {
        Self { app: app.as_weak(), adapter, content, gate, hooks }
    }

    fn serve(&self, url: &Url) -> (StatusCode, HeaderMap, Vec<u8>) {
        let content = self.content.borrow();
        let mut headers = HeaderMap::new();
        headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        let path = url.path();
        if path.starts_with("/msg/") {
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
            if let Ok(csp) = HeaderValue::from_str(&content.csp) {
                headers.insert(header::CONTENT_SECURITY_POLICY, csp);
            }
            return (StatusCode::OK, headers, content.document.as_bytes().to_vec());
        }
        if let Some(cid) = path.strip_prefix("/cid/") {
            let cid = mailcore::mime::percent_decode(cid);
            if let Some((mime, data)) = content.inline_parts.get(&cid) {
                if let Ok(ct) = HeaderValue::from_str(mime) {
                    headers.insert(header::CONTENT_TYPE, ct);
                }
                return (StatusCode::OK, headers, data.clone());
            }
        }
        (StatusCode::NOT_FOUND, headers, Vec::new())
    }
}

pub fn is_content_url(url: &Url) -> bool {
    url.as_str().starts_with(CONTENT_ORIGIN) || url.as_str() == "about:blank"
}

impl WebViewDelegate for AppDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
        if let Some(app) = self.app.upgrade() {
            self.adapter.update_web_content_with_latest_frame(&app);
        }
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        if status == LoadStatus::Complete && !self.gate.ready.replace(true)
            && let Some(url) = self.gate.pending.borrow_mut().take() {
                webview.load(url);
            }
    }

    fn request_navigation(&self, _webview: WebView, request: NavigationRequest) {
        if is_content_url(&request.url) {
            request.allow();
        } else {
            let url = request.url.clone();
            request.deny();
            (self.hooks.on_link)(url);
        }
    }

    /// Every fetch passes through here, including the main document and `data:`
    /// URLs. Only our own content is served; remote loads are cancelled unless
    /// the user allowed remote images for this message.
    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        if url.as_str().starts_with(CONTENT_ORIGIN) {
            let (status, headers, body) = self.serve(&url);
            let mut intercepted = load.intercept(WebResourceResponse::new(url).status_code(status).headers(headers));
            intercepted.send_body_data(body);
            intercepted.finish();
            return;
        }
        let allowed = match url.scheme() {
            "data" | "about" => true,
            "http" | "https" => self.content.borrow().allow_remote,
            _ => false,
        };
        if allowed {
            // Dropping the load lets Servo fetch it normally (CSP still applies).
            drop(load);
        } else {
            log::debug!("blocked load of {url}");
            load.intercept(WebResourceResponse::new(url).status_code(StatusCode::FORBIDDEN)).cancel();
        }
    }

    /// Popups (`target=_blank`, `window.open`) are never created; dropping the
    /// request denies it.
    fn request_create_new(&self, _parent: WebView, _request: CreateNewWebViewRequest) {}

    fn notify_status_text_changed(&self, _webview: WebView, status: Option<String>) {
        (self.hooks.on_status)(status);
    }

    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        if let Some(app) = self.app.upgrade() {
            let kind = match cursor {
                Cursor::Pointer => 1,
                Cursor::Text | Cursor::VerticalText => 2,
                _ => 0,
            };
            app.global::<WebviewLogic>().set_cursor_kind(kind);
        }
    }
}
