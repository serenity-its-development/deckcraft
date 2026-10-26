//! The OneOS host bridge: `postMessage` to the ONE parent the frame page declared. The frame page's loader
//! (`bootstrap.js`) sets `globalThis.__craftHost = {app, version, parent}` from the page's `data-parent`; the editor
//! lives on its own origin, so the parent is a DIFFERENT origin. Messages go to that origin only, and only the parent
//! window on that origin is heard. Every message is a plain object with a `type`; `session` is the per-open nonce the
//! host mints in `craft:open` and every later message echoes
//! (`plans/mona/craft-integration-design-2026-10-25.md` sections 2.2 and 12):
//!
//! - `craft:ready {app, version}` (to the parent): listening; send the file now.
//! - `craft:open {session, name, mime, bytes, versionId, readOnly, collab}` (from the parent): open these bytes. With
//!   `collab` the file is also a live document: the host then sends `craft:crdt:seed` or `craft:crdt:state`.
//! - `craft:save {session, name, mime, bytes, baseVersionId, saveAs, checkpoint?}` (to the parent): the person saved (or
//!   the room asked for a checkpoint: `checkpoint` echoes the request). The bytes are a transferred `ArrayBuffer`; the host
//!   makes a NEW version of the file through the compare-and-set and answers `craft:saved {session, versionId}` (the next
//!   save's base) or `craft:refused {session, code, reason}`. Anything that is not the opened file under its own name (an
//!   export, a Save As) goes as `saveAs: true`, a copy, never a new version of the opened file.
//! - `craft:dirty {session, dirty}` (to the parent): unsaved changes appeared or went away.
//! - `craft:failed {error}` (to the parent): the editor could not start.
//! - `craft:crdt:*` both ways: the live editing channel ([`crate::collab`]).
//! - `craft:media:need {session, hashes, have}` (to the parent) and `craft:media:file {session, name, versionId, bytes |
//!   unchanged}` (from the parent): the pictures other people inserted are in the room only as content addresses; their bytes
//!   are in the file's saved versions, which the host reads through the view lane. `craft:media:publish {session}` asks the
//!   host to land a version soon after this window inserted a picture.
//! - `craft:command {session, id, method, params}` (from the parent) is the assistant's run (`command.batch`): the shell runs
//!   it through the app and answers `craft:result {session, id, ok, result | error}`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use deckcraft_ui_egui::Services;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// What the host's last `craft:open` said.
#[derive(Clone, Default)]
pub struct Opened {
    pub session: String,
    pub name: String,
    pub version_id: String,
    pub read_only: bool,
    pub collab: bool,
}

/// What the host said that the shell acts on, in order.
pub enum Event {
    /// A `craft:open` arrived: open these bytes.
    Opened(Opened, Vec<u8>),
    /// `craft:crdt:<kind>` with its body.
    Crdt { kind: String, body: serde_json::Value },
    /// `craft:refused`.
    Refused { code: String, reason: String },
    /// `craft:media:file`: the file's latest saved version (`bytes`), which carries the pictures other people inserted; or
    /// `unchanged` (the version the frame already searched).
    MediaFile { name: String, version_id: String, bytes: Option<Vec<u8>> },
    /// `craft:command`: the assistant's run, to answer with [`Bridge::answer`].
    Command { id: serde_json::Value, method: String, params: serde_json::Value },
}

#[derive(Default)]
struct Shared {
    opened: Opened,
    events: VecDeque<Event>,
    /// How many `craft:saved` the host has sent: a version landed.
    saved: u64,
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    /// The declared parent origin: the only target and the only sender.
    origin: String,
    app: String,
    version: String,
    shared: Rc<RefCell<Shared>>,
}

/// `scheme://host[:port]` exactly: https, or http only on localhost (the dev estate).
fn plain_origin(v: &str) -> bool {
    let Some((scheme, rest)) = v.split_once("://") else { return false };
    let local = rest == "localhost" || rest.starts_with("localhost:") || rest.starts_with("127.0.0.1");
    (scheme == "https" || (scheme == "http" && local)) && !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
}

impl Bridge {
    /// `None` when the page is not framed, or declares no parent origin.
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let declared = js_sys::Reflect::get(&window, &JsValue::from_str("__craftHost")).ok().filter(|d| d.is_object())?;
        let origin = string(&declared, "parent").filter(|o| plain_origin(o))?;
        Some(Self {
            window,
            parent,
            origin,
            app: string(&declared, "app").unwrap_or_else(|| "slides".into()),
            version: string(&declared, "version").unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
            shared: Rc::default(),
        })
    }

    /// Hand saved and exported files to the host instead of downloading them, report unsaved changes to it, and hear the
    /// files and the live channel it sends.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx);
        let bridge = self.clone();
        services.host_save = Some(Box::new(move |name: &str, bytes: &[u8], save_as: bool| bridge.post_save(name, bytes, save_as, None)));
        // Anything else that would download is a copy: a new file, never a version of the opened one.
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes, true, None) {
                log::error!("handing {name} to the host failed: {e}");
            }
        }));
        let bridge = self.clone();
        // The last (session, dirty) the host was told: a new open starts again.
        let reported: Rc<RefCell<(String, Option<bool>)>> = Rc::default();
        services.on_dirty = Some(Box::new(move |dirty: bool| {
            let session = bridge.shared.borrow().opened.session.clone();
            if session.is_empty() {
                return;
            }
            let mut last = reported.borrow_mut();
            if last.0 != session {
                *last = (session.clone(), None);
            }
            if last.1 != Some(dirty) && bridge.send(&[("type", "craft:dirty".into()), ("session", session.as_str().into()), ("dirty", dirty.into())]) {
                last.1 = Some(dirty);
            }
        }));
    }

    pub fn opened(&self) -> Opened {
        self.shared.borrow().opened.clone()
    }

    pub fn take_events(&self) -> Vec<Event> {
        self.shared.borrow_mut().events.drain(..).collect()
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "craft:ready".into()), ("app", self.app.as_str().into()), ("version", self.version.as_str().into())]);
    }

    /// The app couldn't start (no WebGPU or WebGL2, for example): let the host say so.
    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "craft:failed".into()), ("error", error.into())]);
    }

    /// Send a live-editing frame: `craft:crdt:<kind>` with `body`'s members beside the session.
    pub fn post_crdt(&self, kind: &str, body: &serde_json::Value) {
        let session = self.shared.borrow().opened.session.clone();
        let Ok(json) = serde_json::to_string(body) else { return };
        let Ok(parsed) = js_sys::JSON::parse(&json) else { return };
        let _ = js_sys::Reflect::set(&parsed, &JsValue::from_str("type"), &JsValue::from_str(&format!("craft:crdt:{kind}")));
        let _ = js_sys::Reflect::set(&parsed, &JsValue::from_str("session"), &JsValue::from_str(&session));
        let _ = self.parent.post_message(&parsed, &self.origin);
    }

    /// Answer a relayed `craft:command`: `craft:result {session, id, ok, result | error}`.
    pub fn answer(&self, id: &serde_json::Value, outcome: Result<serde_json::Value, String>) {
        let session = self.shared.borrow().opened.session.clone();
        let Ok(id_js) = js_sys::JSON::parse(&id.to_string()) else { return };
        let mut fields: Vec<(&str, JsValue)> = vec![("type", "craft:result".into()), ("session", session.as_str().into()), ("id", id_js)];
        match outcome {
            Ok(result) => {
                fields.push(("ok", true.into()));
                if let Ok(r) = js_sys::JSON::parse(&result.to_string()) {
                    fields.push(("result", r));
                }
            }
            Err(error) => {
                fields.push(("ok", false.into()));
                fields.push(("error", error.as_str().into()));
            }
        }
        self.send(&fields);
    }

    /// How many versions the host has landed since this page opened.
    pub fn saved_count(&self) -> u64 {
        self.shared.borrow().saved
    }

    /// `craft:media:need {session, hashes, have}`: the shared document uses pictures this window has no bytes for (their content
    /// addresses); the host answers with the file's latest saved version, which carries them, unless it is still `have`.
    pub fn post_media_need(&self, hashes: &[String], have: &str) {
        let session = self.shared.borrow().opened.session.clone();
        let list = js_sys::Array::new();
        for h in hashes {
            list.push(&JsValue::from_str(h));
        }
        self.send(&[("type", "craft:media:need".into()), ("session", session.as_str().into()), ("hashes", list.into()), ("have", have.into())]);
    }

    /// `craft:media:publish {session}`: this window inserted a picture; the other windows can only get its bytes from a saved
    /// version of the file, so the host is asked to land one soon.
    pub fn post_media_publish(&self) {
        let session = self.shared.borrow().opened.session.clone();
        self.send(&[("type", "craft:media:publish".into()), ("session", session.as_str().into())]);
    }

    /// Save (or a checkpoint, when `checkpoint` names the room's request) as a message the host turns into a new version.
    pub fn post_save(&self, path: &str, bytes: &[u8], save_as: bool, checkpoint: Option<u64>) -> Result<(), String> {
        let (session, base, read_only, opened_name) = {
            let s = self.shared.borrow();
            (s.opened.session.clone(), s.opened.version_id.clone(), s.opened.read_only, s.opened.name.clone())
        };
        if session.is_empty() {
            return Err("the host has not opened a document yet".into());
        }
        if read_only {
            return Err("this file is open to view; saving is off".into());
        }
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let mut fields: Vec<(&str, JsValue)> = vec![
            ("type", "craft:save".into()),
            ("session", session.as_str().into()),
            ("name", name.as_str().into()),
            ("mime", mime_of(&name).into()),
            ("baseVersionId", base.as_str().into()),
            ("saveAs", (save_as || name != opened_name).into()),
            ("bytes", buffer.clone().into()),
        ];
        if let Some(c) = checkpoint {
            fields.push(("checkpoint", JsValue::from_f64(c as f64)));
        }
        let message = message(&fields).ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    /// Hear the host.
    fn listen(&self, ctx: egui::Context) {
        let bridge = self.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != bridge.origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &bridge.parent)) {
                return;
            }
            let data = event.data();
            let Some(kind) = string(&data, "type") else { return };
            let known = bridge.shared.borrow().opened.session.clone();
            match kind.as_str() {
                "craft:open" => {
                    let Some(session) = string(&data, "session").filter(|s| !s.is_empty()) else { return };
                    let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Presentation.pptx".to_string());
                    let Some(bytes) = bytes(&data) else { return };
                    let opened = Opened {
                        session,
                        name,
                        version_id: string(&data, "versionId").unwrap_or_default(),
                        read_only: boolean(&data, "readOnly"),
                        collab: boolean(&data, "collab"),
                    };
                    let mut s = bridge.shared.borrow_mut();
                    s.opened = opened.clone();
                    s.events.push_back(Event::Opened(opened, bytes));
                }
                "craft:saved" if string(&data, "session").as_deref() == Some(known.as_str()) => {
                    let mut shared = bridge.shared.borrow_mut();
                    shared.saved += 1;
                    if let Some(version) = string(&data, "versionId") {
                        shared.opened.version_id = version;
                    }
                }
                "craft:media:file" if string(&data, "session").as_deref() == Some(known.as_str()) => {
                    let version_id = string(&data, "versionId").unwrap_or_default();
                    let name = string(&data, "name").unwrap_or_default();
                    let bytes = if boolean(&data, "unchanged") { None } else { bytes(&data) };
                    bridge.shared.borrow_mut().events.push_back(Event::MediaFile { name, version_id, bytes });
                }
                "craft:refused" if string(&data, "session").as_deref() == Some(known.as_str()) => {
                    let code = string(&data, "code").unwrap_or_default();
                    let reason = string(&data, "reason").unwrap_or_else(|| "refused".into());
                    bridge.shared.borrow_mut().events.push_back(Event::Refused { code, reason });
                }
                "craft:command" => {
                    let json_of = |key: &str| -> serde_json::Value {
                        js_sys::Reflect::get(&data, &JsValue::from_str(key))
                            .ok()
                            .and_then(|v| js_sys::JSON::stringify(&v).ok())
                            .and_then(|s| s.as_string())
                            .and_then(|s| serde_json::from_str(&s).ok())
                            .unwrap_or(serde_json::Value::Null)
                    };
                    let method = string(&data, "method").unwrap_or_default();
                    bridge.shared.borrow_mut().events.push_back(Event::Command { id: json_of("id"), method, params: json_of("params") });
                }
                other => {
                    let Some(rest) = other.strip_prefix("craft:crdt:") else { return };
                    if string(&data, "session").as_deref() != Some(known.as_str()) {
                        return;
                    }
                    let body = js_sys::JSON::stringify(&data).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null);
                    bridge.shared.borrow_mut().events.push_back(Event::Crdt { kind: rest.to_owned(), body });
                }
            }
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

/// The content type of a file name the editor saves.
fn mime_of(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("potx") => "application/vnd.openxmlformats-officedocument.presentationml.template",
        Some("ppsx") => "application/vnd.openxmlformats-officedocument.presentationml.slideshow",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("txt") => "text/plain",
        Some("rtf") => "application/rtf",
        _ => "application/octet-stream",
    }
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

fn boolean(data: &JsValue, key: &str) -> bool {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok().and_then(|v| v.as_bool()).unwrap_or(false)
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
