//! Host modes: a page that runs CADCraft opens drawings in it and stores what it saves instead
//! of the browser downloading. The messages and the two transports are `craft-web-host`'s
//! (storytold/craft-libs), the same contract as the other Craft apps' bridges, with the
//! `cadcraft:` prefix. This module is CADCraft's part of it.
//!
//! - `?host=parent` (an `<iframe>`, such as a Nextcloud app) talks to the same-origin parent over
//!   `postMessage`; `data-host="page"` on the canvas (the page's own document, no iframe) uses
//!   DOM `CustomEvent`s on the canvas instead. `?host=parent` wins when both are present. With no
//!   parent, or a parent on another origin, there is no bridge: the normal app with downloads.
//! - `cadcraft:ready` once the app is listening; send a drawing now. CADCraft reads bytes, so it
//!   says `file: false` and a host sends `bytes`.
//! - `cadcraft:open` (from the host): `{ name, bytes }`. It opens like a file picked with
//!   File › Open or dropped on the window; an oversized drawing is refused before it is copied.
//!   A drawing file (DXF, or a name with no extension) that opens keeps the host's name as its
//!   file, as a drawing opened from disk does: File › Save then hands it back under that name,
//!   with no Save As, in the DXF version it came in (or the oldest CADCraft writes, R2000, for an
//!   older file) and binary if it was binary. An imported SVG stays untitled. An SHX font (a
//!   `.shx` name) is registered like a picked one, not opened, and gets no `cadcraft:failed`,
//!   unless it is refused before it is read (sent as a `File`, or over the size limit).
//! - `cadcraft:save` (to the host): `{ name, saveAs, bytes }` for every file CADCraft would
//!   otherwise download (SAVE, SAVEAS, exports): it replaces the engine's delivery hook. The host
//!   stores the file and reports failures itself. CADCraft doesn't tell the host which saves
//!   make a file the document, so `saveAs` is always `false`.
//! - `cadcraft:dirty` whenever unsaved changes appear or go away; `cadcraft:failed` when the app
//!   couldn't start, and when a drawing the host sent can't be opened (a DWG, which the web build
//!   doesn't read, a damaged or an oversized file), with the reason the app also shows.
//!
//! In both host modes the app starts on the Start tab rather than an empty Drawing1, and the
//! host guards leaving the page.

use std::cell::RefCell;
use std::rc::Rc;

use cadcraft_io::Format;
use cadcraft_ui_egui::CadApp;
use craft_web_host::web::{FileData, Host, Message};

use crate::upload::HostOpened;

/// The app id in the host messages (`cadcraft:ready`).
const APP: &str = "cadcraft";

/// The name a drawing gets when the host sent none (the crate says `document` for a blank name).
const DEFAULT_NAME: &str = "Drawing.dxf";

thread_local! {
    /// The bridge [`deliver`] posts through (the engine's hook is a plain `fn`).
    static BRIDGE: RefCell<Option<Bridge>> = const { RefCell::new(None) };
}

#[derive(Clone)]
pub struct Bridge {
    host: Rc<Host>,
}

impl Bridge {
    /// The bridge for this page, or `None` when CADCraft isn't hosted: no `?host=parent` and no
    /// `data-host="page"` on the canvas, an unframed `?host=parent` page (it stays the normal app
    /// with downloads), or a parent on another origin.
    pub fn connect(canvas: &web_sys::HtmlCanvasElement) -> Option<Self> {
        Some(Self { host: Rc::new(Host::connect(APP, canvas)?) })
    }

    /// Make this the bridge [`deliver`] uses.
    pub fn install(&self) {
        BRIDGE.with(|b| *b.borrow_mut() = Some(self.clone()));
    }

    pub fn post_ready(&self) {
        let _ = self.host.post_ready(env!("CARGO_PKG_VERSION"), false);
    }

    pub fn post_failed(&self, error: &str) {
        let _ = self.host.post_failed(error, None);
    }

    /// Hear drawings from the host; they arrive through the upload inbox.
    pub fn listen(&self, inbox: crate::upload::Inbox) {
        self.host.listen(move |message| {
            if let Message::Open { name, data } = message {
                receive_open(&name, &data, &inbox);
            }
        });
    }

    /// After the inbox opened what the host sent: tell the host about each drawing that could not
    /// be opened, and give each one that did the host's name as its file (see [`bind_file`]).
    pub fn opened(&self, app: &mut CadApp, outcomes: Vec<HostOpened>) {
        for outcome in outcomes {
            match outcome {
                HostOpened::Drawing { index, name } => bind_file(app, index, &name),
                HostOpened::Failed(error) => self.post_failed(&error),
            }
        }
    }

    /// After each logic callback (including a hidden tab's): tell the host when unsaved changes
    /// appear or go away. The crate does not repeat a state it already reported, so this is
    /// idempotent.
    pub fn report_dirty(&self, app: &CadApp) {
        self.host.post_dirty(app.session.docs.iter().any(|d| d.is_dirty()));
    }

    fn post_save(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.host.post_save(name, bytes, None, false)
    }
}

/// The engine's delivery hook in host mode ([`cadcraft_engine::cmd::file::set_deliver`]): hand
/// the file to the host page. `Ok` once it was sent; the engine then counts a drawing as saved.
pub fn deliver(name: &str, _mime: &str, bytes: &[u8]) -> Result<(), String> {
    let bridge = BRIDGE.with(|b| b.borrow().clone()).ok_or("the page that embeds CADCraft is gone")?;
    bridge.post_save(name, bytes)
}

/// Give the drawing at `index`, opened from the host's file `name`, that name as its file, as
/// OPEN does for a file read from disk. Without it the drawing is untitled, so its first Save runs
/// Save As in the default save version (2018) and a DXF kept at an older version for another
/// program comes back silently upgraded. With it, QSAVE writes the drawing as it was read: the
/// version in its `$ACADVER`, binary if it was binary, under the same name. On the web a file is
/// only a name: the save is still handed to the host, never written anywhere. Only DXF names
/// (a name without an extension counts as DXF) get one. An imported SVG stays untitled, so Save
/// asks for a DXF name instead of writing over the picture. A `.dwg` or `.dwt` name stays
/// untitled too: such a file only opens here when its bytes are really DXF, and QSAVE under that
/// name would send it to the DWG writer, which the web build lacks, so the save would fail where
/// Save As to a DXF name works.
fn bind_file(app: &mut CadApp, index: usize, name: &str) {
    if Format::from_path(name) != Some(Format::Dxf) {
        return;
    }
    if let Some(drawing) = app.session.docs.get_mut(index)
        && drawing.path.is_none()
    {
        drawing.path = Some(name.to_string());
    }
}

/// The `cadcraft:open` payload, `{ name, bytes }` where `bytes` holds a drawing: it opens like a
/// file picked with File › Open or dropped on the window. The crate has cleaned the name (and cut
/// it to 255 characters); a blank one comes as `document` and becomes `Drawing.dxf`. An oversized
/// drawing is refused before it is copied.
fn receive_open(name: &str, data: &FileData, inbox: &crate::upload::Inbox) {
    let name = if name == "document" { DEFAULT_NAME } else { name }.to_string();
    let FileData::Bytes(array) = data else {
        inbox.refuse(format!("Open: {name} came as a File; CADCraft reads bytes"));
        return;
    };
    // Refuse an oversized drawing before copying it into the app's memory (the data's own length).
    if let Some(e) = CadApp::open_size_error(&name, data.size() as u64) {
        inbox.refuse(e);
        return;
    }
    inbox.receive(name, array.to_vec());
}
