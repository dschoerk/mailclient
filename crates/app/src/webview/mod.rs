// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

//! Servo webview for Slint, adapted from Slint's `examples/servo`
//! (https://github.com/slint-ui/slint/tree/master/examples/servo).
//! Used here only to display mail content; see `delegate.rs` for the security policy.

mod adapter;
pub mod delegate;
mod events_utils;
mod rendering_context;
mod waker;
mod webview;
mod webview_events;

pub use adapter::SlintServoAdapter;
pub use delegate::AppDelegate;
pub use rendering_context::ServoRenderingAdapter;
pub use waker::Waker;
pub use webview::MailView;
pub use webview_events::WebViewEvents;
