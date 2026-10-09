use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use mailcore::mime::{self, Draft};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::format;
use crate::{AppWindow, AttachmentItem};

type SendFn = Box<dyn Fn(Draft, Option<(String, u32)>) -> u64>;
type CloseFn = Box<dyn Fn(&Composer)>;

/// The draft being written. It is shown inside the main window (replacing the reader),
/// because Android has a single window surface; one draft at a time.
pub struct Composer {
    app: slint::Weak<AppWindow>,
    attachments: RefCell<Vec<PathBuf>>,
    attachment_model: Rc<VecModel<AttachmentItem>>,
    replied_to: Option<(String, u32)>,
    /// In-Reply-To and References from the reply template.
    threading: (String, String),
    /// Id of the send in flight, if any.
    send_id: Cell<Option<u64>>,
    on_send: SendFn,
    on_close: CloseFn,
}

impl Composer {
    pub fn open(
        app: &AppWindow,
        draft: Draft,
        replied_to: Option<(String, u32)>,
        on_send: SendFn,
        on_close: CloseFn,
    ) -> Rc<Composer> {
        app.set_compose_to(draft.to.clone().into());
        app.set_compose_cc(draft.cc.clone().into());
        app.set_compose_bcc(draft.bcc.clone().into());
        app.set_compose_subject(draft.subject.clone().into());
        app.set_compose_body(draft.body.clone().into());
        app.set_compose_error("".into());
        app.set_compose_sending(false);

        let composer = Rc::new(Composer {
            app: app.as_weak(),
            attachments: RefCell::new(draft.attachments.clone()),
            attachment_model: Rc::new(VecModel::default()),
            replied_to,
            threading: (draft.in_reply_to, draft.references),
            send_id: Cell::new(None),
            on_send,
            on_close,
        });
        app.set_compose_attachments(ModelRc::from(composer.attachment_model.clone()));
        composer.refresh_attachments();

        let weak = Rc::downgrade(&composer);
        app.on_compose_send(move || {
            if let Some(c) = weak.upgrade() {
                c.send();
            }
        });
        let weak = Rc::downgrade(&composer);
        app.on_compose_attach(move || {
            if let Some(c) = weak.upgrade() {
                c.attach();
            }
        });
        let weak = Rc::downgrade(&composer);
        app.on_compose_remove_attachment(move |index| {
            if let Some(c) = weak.upgrade() {
                let mut list = c.attachments.borrow_mut();
                if (index as usize) < list.len() {
                    list.remove(index as usize);
                }
                drop(list);
                c.refresh_attachments();
            }
        });
        let weak = Rc::downgrade(&composer);
        app.on_compose_discard(move || {
            if let Some(c) = weak.upgrade() {
                c.close();
            }
        });
        app.set_composing(true);
        composer
    }

    fn app(&self) -> AppWindow {
        self.app.upgrade().expect("main window alive")
    }

    pub fn id(&self) -> Option<u64> {
        self.send_id.get()
    }

    pub fn close(&self) {
        self.app().set_composing(false);
        (self.on_close)(self);
    }

    pub fn send_failed(&self, error: &str) {
        self.send_id.set(None);
        let app = self.app();
        app.set_compose_sending(false);
        app.set_compose_error(error.into());
    }

    fn draft(&self) -> Draft {
        let app = self.app();
        Draft {
            to: app.get_compose_to().into(),
            cc: app.get_compose_cc().into(),
            bcc: app.get_compose_bcc().into(),
            subject: app.get_compose_subject().into(),
            body: app.get_compose_body().into(),
            in_reply_to: self.threading.0.clone(),
            references: self.threading.1.clone(),
            attachments: self.attachments.borrow().clone(),
            ..Default::default()
        }
    }

    fn send(&self) {
        if self.send_id.get().is_some() {
            return;
        }
        let draft = self.draft();
        let app = self.app();
        // Catch obvious mistakes (no recipient, unreadable attachment) before queueing.
        let mut probe = draft.clone();
        probe.from_addr = "probe@localhost".into();
        if let Err(e) = mime::build_message(&probe) {
            app.set_compose_error(e.to_string().into());
            return;
        }
        app.set_compose_error("".into());
        app.set_compose_sending(true);
        let id = (self.on_send)(draft, self.replied_to.clone());
        self.send_id.set(Some(id));
    }

    /// shortcut: no file picker on Android yet (needs the Storage Access Framework via JNI).
    #[cfg(target_os = "android")]
    fn attach(self: &Rc<Self>) {
        self.app().set_compose_error("Attaching files is not supported on Android yet".into());
    }

    #[cfg(not(target_os = "android"))]
    fn attach(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let _ = slint::spawn_local(async move {
            let Some(files) = rfd::AsyncFileDialog::new().pick_files().await else { return };
            if let Some(c) = weak.upgrade() {
                c.attachments.borrow_mut().extend(files.iter().map(|f| f.path().to_path_buf()));
                c.refresh_attachments();
            }
        });
    }

    fn refresh_attachments(&self) {
        let items: Vec<AttachmentItem> = self
            .attachments
            .borrow()
            .iter()
            .enumerate()
            .map(|(i, p)| AttachmentItem {
                index: i as i32,
                name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into(),
                size: std::fs::metadata(p).map(|m| format::size(m.len() as usize)).unwrap_or_default().into(),
            })
            .collect();
        self.attachment_model.set_vec(items);
    }
}
