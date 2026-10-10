use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use mailcore::mime::{self, Draft};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::format;
use crate::{AppWindow, AttachmentItem};

type SendFn = Box<dyn Fn(Draft, Option<(String, u32)>) -> u64>;
type SaveFn = Box<dyn Fn(Draft) -> u64>;
/// Called once the composer is gone; `true` when the draft was discarded.
type CloseFn = Box<dyn Fn(&Composer, bool)>;

/// Unsaved changes reach Drafts at most this late (a killed app loses no more).
const AUTOSAVE: std::time::Duration = std::time::Duration::from_secs(15);

/// The draft being written. It is shown inside the main window (replacing the reader),
/// because Android has a single window surface; one draft at a time.
pub struct Composer {
    app: slint::Weak<AppWindow>,
    attachments: RefCell<Vec<PathBuf>>,
    attachment_model: Rc<VecModel<AttachmentItem>>,
    replied_to: Option<(String, u32)>,
    /// In-Reply-To, References and Message-ID, kept from the template or reopened draft.
    threading: (String, String, String),
    /// Id of the send in flight, if any.
    send_id: Cell<Option<u64>>,
    /// The draft as Drafts has it (or as opened).
    saved: RefCell<Draft>,
    /// Save in flight: id, content, and whether to close once it is stored.
    saving: RefCell<Option<(u64, Draft, bool)>>,
    autosave: slint::Timer,
    on_send: SendFn,
    on_save: SaveFn,
    on_close: CloseFn,
}

impl Composer {
    pub fn open(
        app: &AppWindow,
        draft: Draft,
        replied_to: Option<(String, u32)>,
        on_send: SendFn,
        on_save: SaveFn,
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
            threading: (draft.in_reply_to.clone(), draft.references.clone(), draft.message_id.clone()),
            send_id: Cell::new(None),
            saved: RefCell::new(draft),
            saving: RefCell::new(None),
            autosave: slint::Timer::default(),
            on_send,
            on_save,
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
                c.finish(true);
            }
        });
        let weak = Rc::downgrade(&composer);
        app.on_compose_close(move || {
            if let Some(c) = weak.upgrade() {
                c.save(true);
            }
        });
        let weak = Rc::downgrade(&composer);
        composer.autosave.start(slint::TimerMode::Repeated, AUTOSAVE, move || {
            if let Some(c) = weak.upgrade() {
                c.save(false);
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

    /// Closes after sending.
    pub fn close(&self) {
        self.finish(false);
    }

    fn finish(&self, discarded: bool) {
        self.autosave.stop();
        self.app().set_composing(false);
        (self.on_close)(self, discarded);
    }

    pub fn message_id(&self) -> &str {
        &self.threading.2
    }

    /// A send or a save failed. Returns false when `id` is not one of ours.
    pub fn failed(&self, id: u64, error: &str) -> bool {
        let closing = match &*self.saving.borrow() {
            Some((saving, _, closing)) if *saving == id => *closing,
            _ if self.send_id.get() == Some(id) => true,
            _ => return false,
        };
        if self.send_id.get() == Some(id) {
            self.send_id.set(None);
        } else {
            self.saving.borrow_mut().take();
        }
        // An autosave failing is not worth interrupting the writer; the next one retries.
        if closing {
            let app = self.app();
            app.set_compose_sending(false);
            app.set_compose_error(error.into());
        }
        true
    }

    pub fn draft_saved(&self, id: u64) {
        let Some((saving, draft, closing)) = self.saving.borrow().clone() else { return };
        if saving != id {
            return;
        }
        self.saving.borrow_mut().take();
        *self.saved.borrow_mut() = draft;
        if closing {
            // Saves once more if it changed while this save was under way.
            self.save(true);
        }
    }

    /// Stores the draft in Drafts if it changed; `close`: then leave the composer.
    fn save(&self, close: bool) {
        if self.send_id.get().is_some() {
            return;
        }
        let draft = self.draft();
        if let Some((_, _, closing)) = self.saving.borrow_mut().as_mut() {
            // One save at a time; a close waits for the one in flight, then saves again if needed.
            *closing |= close;
            if close {
                self.app().set_compose_sending(true);
            }
            return;
        }
        if draft == *self.saved.borrow() {
            if close {
                self.finish(false);
            }
            return;
        }
        if close {
            let app = self.app();
            app.set_compose_error("".into());
            app.set_compose_sending(true);
        }
        let id = (self.on_save)(draft.clone());
        *self.saving.borrow_mut() = Some((id, draft, close));
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
            message_id: self.threading.2.clone(),
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

    fn attach(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let _ = slint::spawn_local(async move {
            let files = crate::platform::pick_files().await;
            let Some(c) = weak.upgrade() else { return };
            match files {
                Ok(files) => {
                    c.attachments.borrow_mut().extend(files);
                    c.refresh_attachments();
                }
                Err(e) => c.app().set_compose_error(e.to_string().into()),
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
