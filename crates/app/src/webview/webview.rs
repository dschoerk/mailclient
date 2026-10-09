// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT
// Adapted from Slint's examples/servo: loads mail content instead of URLs and
// configures Servo with JavaScript hardening.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use euclid::Scale;
use smol::channel;
use url::Url;
use winit::dpi::PhysicalSize;

use servo::{Preferences, Servo, ServoBuilder, Theme, WebViewBuilder};
use slint::ComponentHandle;

use crate::{
    AppWindow, WebviewLogic,
    webview::{
        AppDelegate, ServoRenderingAdapter, SlintServoAdapter, Waker, WebViewEvents,
        delegate::{Hooks, LoadGate, MessageContent},
    },
};
use mailcore::mime::CONTENT_ORIGIN;

/// Servo-backed message view. Content is served from memory under
/// [`CONTENT_ORIGIN`] with a sandboxing CSP, so scripts never run.
pub struct MailView {
    adapter: Rc<SlintServoAdapter>,
    content: Rc<RefCell<MessageContent>>,
    gate: Rc<LoadGate>,
    generation: Cell<u64>,
}

impl MailView {
    pub fn new(
        app: &AppWindow,
        device: slint::wgpu_30::wgpu::Device,
        queue: slint::wgpu_30::wgpu::Queue,
        hooks: Hooks,
    ) -> Rc<MailView> {
        let (waker_sender, waker_receiver) = channel::unbounded::<()>();
        let adapter = Rc::new(SlintServoAdapter::new(waker_sender, waker_receiver, device, queue));
        let content = Rc::new(RefCell::new(MessageContent::default()));
        let gate = Rc::new(LoadGate::default());

        let rendering_adapter = Self::init_rendering_adapter(app, &adapter);
        let servo = Self::init_servo(&adapter);
        Self::init_webview(app, &adapter, servo, rendering_adapter, content.clone(), gate.clone(), hooks);
        Self::spin_servo_event_loop(&adapter);
        WebViewEvents::new(app, adapter.clone());

        Rc::new(MailView { adapter, content, gate, generation: Cell::new(0) })
    }

    pub fn show(&self, content: MessageContent) {
        *self.content.borrow_mut() = content;
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        // A fresh URL per message so Servo never serves a cached document.
        let url = Url::parse(&format!("{CONTENT_ORIGIN}/msg/{generation}")).expect("valid content URL");
        if self.gate.ready.get() {
            self.adapter.webview().load(url);
        } else {
            *self.gate.pending.borrow_mut() = Some(url);
        }
    }

    pub fn shutdown(&self) {
        self.adapter.shutdown();
    }

    pub fn clear(&self) {
        self.show(MessageContent {
            document: "<!DOCTYPE html><html><body style=\"background:#fff\"></body></html>".into(),
            csp: mailcore::mime::content_security_policy(false),
            ..Default::default()
        });
    }

    fn init_rendering_adapter(app: &AppWindow, adapter: &SlintServoAdapter) -> Rc<Box<dyn ServoRenderingAdapter>> {
        let scale_factor = app.window().scale_factor();
        let width = app.global::<WebviewLogic>().get_viewport_width() * scale_factor;
        let height = app.global::<WebviewLogic>().get_viewport_height() * scale_factor;
        let size = PhysicalSize::new(width.max(1.0) as u32, height.max(1.0) as u32);
        let rendering_adapter =
            super::rendering_context::try_create_gpu_context(adapter.wgpu_device(), adapter.wgpu_queue(), size)
                .expect("creating Servo rendering context");
        Rc::new(rendering_adapter)
    }

    fn init_servo(adapter: &SlintServoAdapter) -> Servo {
        // Scripts are disabled per document by the CSP sandbox. These settings
        // shrink the engine's attack surface in case that layer ever fails.
        let mut prefs = Preferences::default();
        prefs.js_disable_jit = true;
        prefs.js_wasm_enabled = false;
        prefs.dom_webgpu_enabled = false;
        prefs.dom_webgl2_enabled = false;
        prefs.dom_serviceworker_enabled = false;
        prefs.dom_bluetooth_enabled = false;
        prefs.network_http_cache_disabled = true;

        ServoBuilder::default()
            .preferences(prefs)
            .event_loop_waker(Box::new(Waker::new(adapter.waker_sender())))
            .build()
    }

    fn init_webview(
        app: &AppWindow,
        adapter: &Rc<SlintServoAdapter>,
        servo: Servo,
        rendering_adapter: Rc<Box<dyn ServoRenderingAdapter>>,
        content: Rc<RefCell<MessageContent>>,
        gate: Rc<LoadGate>,
        hooks: Hooks,
    ) {
        let delegate = Rc::new(AppDelegate::new(app, adapter.clone(), content, gate, hooks));
        let webview = WebViewBuilder::new(&servo, rendering_adapter.get_rendering_context())
            .url(Url::parse("about:blank").unwrap())
            .delegate(delegate)
            .build();
        webview.show();
        // Mail is designed for light backgrounds; the document forces light too.
        webview.notify_theme_change(Theme::Light);
        webview.set_hidpi_scale_factor(Scale::new(app.window().scale_factor()));
        adapter.set_inner(servo, webview, rendering_adapter);
    }

    fn spin_servo_event_loop(adapter: &Rc<SlintServoAdapter>) {
        let adapter_weak = Rc::downgrade(adapter);
        slint::spawn_local(async move {
            loop {
                let Some(adapter) = adapter_weak.upgrade() else { break };
                let receiver = adapter.waker_receiver();
                drop(adapter);
                if receiver.recv().await.is_err() {
                    break;
                }
                let Some(adapter) = adapter_weak.upgrade() else { break };
                adapter.servo().spin_event_loop();
            }
        })
        .expect("spawning Servo event loop task");
    }
}
