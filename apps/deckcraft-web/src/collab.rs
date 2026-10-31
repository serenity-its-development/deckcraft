//! Live editing for the host bridge: the frame side of the host's common collaboration adapter (design section 12). The shim
//! itself is `deckcraft-collab` (pure, tested natively and against the host's real engine); this module is its wiring: it turns
//! the session's edits into `craft:crdt:ops`, and the host's `state` / `delta` / `seed` / `export` into the deck the editor shows.
//!
//! What goes both ways: slides, shapes and their properties, text with its formatting, speaker notes, comments (through the
//! host's comments tier, as `craft:crdt:comment`), pictures (as content addresses; the bytes come from the file's saved version)
//! and where the other people are (`craft:crdt:cursor` out, `craft:crdt:peer` in: the shapes they have selected).
//!
//! Order each frame: the host's messages that wait for the file to finish opening are held; before a remote change is shown, the
//! person's own edits since the last frame are sent (so none is overwritten); then the remote change is shown; then any edit
//! this frame made is sent. Undo and Redo are the room's (`craft:crdt:undo` / `redo`): the session's own snapshots would also
//! take back what others did.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use deckcraft_collab::media::sha256_hex;
use deckcraft_collab::{Batch, Shim, changes_of};
use deckcraft_engine::RemoteCursor;
use deckcraft_model::Presentation;
use deckcraft_ui_egui::SlideApp;
use serde_json::{Value, json};

use crate::host::{Bridge, Event, Opened};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Not a live document (the host opened it without `collab`, or live editing ended).
    Off,
    /// Opened with `collab`: waiting for the host to seed the room or send its state.
    Joining,
    /// In the room.
    Live,
}

/// Another person in the room: their name, colour and last cursor body (resolved against the deck every frame).
struct Peer {
    name: String,
    color: [u8; 3],
    cursor: Value,
}

/// My selection goes out when it moves (at most this often) and again this often while it stays, so the others' copy of it does
/// not expire (the host drops a cursor 6 s after its last frame).
const CURSOR_MIN_MS: u64 = 120;
const CURSOR_REPEAT_MS: u64 = 3_000;
/// Asking the host for the pictures other people inserted: first after this long, then less often while they are still missing.
const MEDIA_FIRST_MS: u64 = 400;
const MEDIA_MAX_MS: u64 = 120_000;
/// After inserting a picture the host is asked to land a version; asked again this often until one lands.
const PUBLISH_REPEAT_MS: u64 = 20_000;

pub struct Collab {
    bridge: Bridge,
    shim: Shim,
    phase: Phase,
    /// The uid of the presentation the host had opened when its `craft:open` arrived: the file has loaded once the app holds
    /// another.
    baseline: Option<u64>,
    loaded: bool,
    /// The presentation (uid) and deck last handed to (or taken from) the shim.
    doc_uid: u64,
    last_doc: Option<Arc<Presentation>>,
    held: VecDeque<Event>,
    peers: BTreeMap<String, Peer>,
    last_cursor: Option<Value>,
    last_cursor_ms: u64,
    /// Pictures: when to ask next, how long after that, and the version of the file already searched for them.
    media_next_ms: u64,
    media_backoff_ms: u64,
    media_version: String,
    /// A picture was inserted and no version has landed since (`saved` was this many when it was asked).
    publish_at: Option<(u64, u64)>,
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

/// Random bits for keys, from the browser's crypto.
fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    if let Some(crypto) = web_sys::window().and_then(|w| w.crypto().ok()) {
        let _ = crypto.get_random_values_with_u8_array(&mut buf);
    }
    u64::from_le_bytes(buf) ^ (js_sys::Math::random() * 9.007_199_254_740_992e15) as u64
}

/// `#rrggbb` as bytes.
fn rgb_of(hex: &str) -> [u8; 3] {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| h.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok()).unwrap_or(0x7c);
    [byte(0), byte(2), byte(4)]
}

impl Collab {
    pub fn new(bridge: Bridge) -> Self {
        Collab {
            bridge,
            shim: Shim::new(random_seed()),
            phase: Phase::Off,
            baseline: None,
            loaded: false,
            doc_uid: 0,
            last_doc: None,
            held: VecDeque::new(),
            peers: BTreeMap::new(),
            last_cursor: None,
            last_cursor_ms: 0,
            media_next_ms: 0,
            media_backoff_ms: MEDIA_FIRST_MS,
            media_version: String::new(),
            publish_at: None,
        }
    }

    /// A `craft:open` arrived: note it against the presentation the app holds right now (the app opens the file from the inbox
    /// during its pass, so the open has happened once `host_doc` names another).
    pub fn opened(&mut self, o: &Opened, app: &SlideApp) {
        self.phase = if o.collab { Phase::Joining } else { Phase::Off };
        self.baseline = app.host_doc.as_ref().map(|(uid, _)| *uid);
        self.loaded = false;
        // Messages for the file that was open before are not for this one.
        self.held.clear();
        self.peers.clear();
        self.last_cursor = None;
        self.media_version.clear();
        self.publish_at = None;
        self.last_doc = None;
    }

    /// The host sent something for the live channel.
    pub fn event(&mut self, ev: Event) {
        self.held.push_back(ev);
    }

    fn off(&mut self, app: &mut SlideApp, why: &str) {
        self.phase = Phase::Off;
        app.session.collab_undo = false;
        app.session.remote_cursors.clear();
        app.set_status(format!("Live editing is off: {why}"));
    }

    fn go_live(&mut self, app: &mut SlideApp) {
        self.phase = Phase::Live;
        if let Some(d) = app.session.active() {
            self.doc_uid = d.uid;
            self.last_doc = Some(d.doc.clone());
        }
        app.session.collab_undo = true;
    }

    fn post_batches(&self, batches: &[Batch]) {
        for b in batches {
            let mut body = json!({"ops": b.ops, "seq": b.seq, "undo": if b.join { "join" } else { "step" }, "label": b.label});
            if b.seed {
                body["seed"] = json!(true);
            }
            self.bridge.post_crdt("ops", &body);
        }
    }

    /// Send the comment messages the shim holds.
    fn post_comment_msgs(&mut self) {
        for m in self.shim.take_comment_msgs() {
            self.bridge.post_crdt("comment", &m.body());
        }
    }

    /// A batch that put a picture reference in the room: the other windows can only get its bytes from a saved version.
    fn note_pictures(&mut self, batches: &[Batch]) {
        let carries = batches.iter().flat_map(|b| &b.ops).any(|op| op.to_string().contains("\"cas-"));
        if carries && self.publish_at.is_none() {
            self.publish_at = Some((0, self.bridge.saved_count()));
        }
    }

    /// Send what the person changed since the last frame.
    fn flush_local(&mut self, app: &mut SlideApp) {
        if self.phase != Phase::Live {
            return;
        }
        let Some(d) = app.session.active() else { return };
        if d.uid != self.doc_uid {
            self.off(app, "another presentation was opened in this window");
            return;
        }
        if self.last_doc.as_ref().is_some_and(|last| Arc::ptr_eq(last, &d.doc)) {
            return;
        }
        let deck = d.doc.clone();
        self.last_doc = Some(deck.clone());
        if self.bridge.opened().read_only {
            return;
        }
        match self.shim.local(&deck, now_ms()) {
            Ok(batches) => {
                self.post_batches(&batches);
                self.note_pictures(&batches);
                self.shim.local_comments(&deck);
                self.post_comment_msgs();
            }
            Err(e) => self.off(app, &e.to_string()),
        }
    }

    /// Show what the shim read: the deck is changed in place (no undo step, no unsaved-state change of its own).
    fn show(
        &mut self,
        app: &mut SlideApp,
        f: impl FnOnce(&mut Shim, &mut Presentation) -> Result<bool, deckcraft_collab::ShimError>,
    ) -> Result<(), String> {
        let mut outcome: Result<bool, deckcraft_collab::ShimError> = Ok(false);
        let shim = &mut self.shim;
        app.session.change_remote(|deck| outcome = f(shim, deck)).map_err(|e| e.to_string())?;
        outcome.map_err(|e| e.to_string())?;
        if let Some(d) = app.session.active() {
            self.last_doc = Some(d.doc.clone());
        }
        Ok(())
    }

    fn handle(&mut self, app: &mut SlideApp, kind: &str, body: &Value) {
        match kind {
            "state" => {
                let doc = body.get("state").and_then(|s| s.get("doc")).cloned().unwrap_or(Value::Null);
                // An empty room has nothing to show: the host asks for the seed.
                if doc.as_object().is_none_or(|o| o.get("slides").is_none()) {
                    return;
                }
                self.flush_local(app);
                let comments = body.get("state").and_then(|s| s.get("comments")).cloned();
                let shown = self.show(app, |shim, deck| {
                    shim.remote_state(&doc, deck)?;
                    if let Some(c) = &comments {
                        shim.remote_comments(c);
                    }
                    shim.apply_comments(deck)
                });
                match shown {
                    Ok(()) => {
                        self.post_comment_msgs();
                        self.go_live(app);
                    }
                    Err(e) => self.off(app, &e),
                }
            }
            "seed" => {
                let Some(deck) = app.session.active().map(|d| d.doc.clone()) else { return };
                match self.shim.seed(&deck, now_ms()) {
                    Ok(batches) => {
                        // The opened file already carries its pictures: nothing to publish for a seed.
                        self.post_batches(&batches);
                        self.shim.local_comments(&deck);
                        self.post_comment_msgs();
                        self.go_live(app);
                    }
                    Err(e) => self.off(app, &e.to_string()),
                }
            }
            "delta" => {
                if self.phase != Phase::Live {
                    return;
                }
                self.flush_local(app);
                if self.phase != Phase::Live {
                    return;
                }
                let ack = body.get("ack").and_then(Value::as_u64).unwrap_or(0);
                // The shim reads what is below `doc`, and the host's comments as one value at `comments`.
                let all = changes_of(body.get("changes").unwrap_or(&Value::Null));
                self.shim.remote_comment_changes(&all);
                let doc_changes: Vec<_> = all
                    .into_iter()
                    .filter_map(|mut c| {
                        (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                            c.path.remove(0);
                            c
                        })
                    })
                    .collect();
                let shown = self.show(app, |shim, deck| {
                    let a = shim.remote_delta(&doc_changes, ack, now_ms(), deck)?;
                    let b = shim.apply_comments(deck)?;
                    Ok(a || b)
                });
                if let Err(e) = shown {
                    self.off(app, &e);
                    return;
                }
                self.post_comment_msgs();
            }
            "export" => {
                self.flush_local(app);
                let request = body.get("request").and_then(Value::as_u64);
                let opened = self.bridge.opened();
                let Some(deck) = app.session.active().map(|d| d.doc.clone()) else { return };
                let ext = std::path::Path::new(&opened.name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                let format = match ext.as_str() {
                    "potx" | "ppsx" | "deckcraft" => ext.as_str(),
                    _ => "pptx",
                };
                // A .ppt is never written back: the checkpoint goes to <stem>.pptx (files/973, 975).
                let legacy = deckcraft_engine::cmd::file::is_legacy_name(&opened.name);
                let name = if legacy { deckcraft_engine::cmd::file::pptx_beside(&opened.name) } else { opened.name.clone() };
                match deckcraft_engine::cmd::file::save_bytes(&deck, format) {
                    Ok(bytes) => {
                        if let Err(e) = self.bridge.post_save(&name, &bytes, legacy, request) {
                            app.set_status(format!("The checkpoint was not sent: {e}"));
                        }
                    }
                    Err(e) => app.set_status(format!("The checkpoint could not be made: {e}")),
                }
            }
            "peer" => {
                let Some(party) = body.get("party").and_then(Value::as_str) else { return };
                match body.get("cursor") {
                    Some(c) if !c.is_null() => {
                        let name = body.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()).unwrap_or("Someone").to_owned();
                        let color = rgb_of(body.get("color").and_then(Value::as_str).unwrap_or("#7c3aed"));
                        self.peers.insert(party.to_owned(), Peer { name, color, cursor: c.clone() });
                    }
                    _ => {
                        self.peers.remove(party);
                    }
                }
            }
            "unplaced" => app.set_status("Some of your offline edits could not be placed in the live presentation; they are shown beside it."),
            _ => {}
        }
    }

    /// The host sent the file's latest saved version: the pictures other people inserted are in it.
    fn media_file(&mut self, app: &mut SlideApp, name: &str, version_id: &str, bytes: Option<Vec<u8>>) {
        if !version_id.is_empty() {
            self.media_version = version_id.to_owned();
        }
        let Some(bytes) = bytes else { return };
        let wanted: std::collections::BTreeSet<String> = self.shim.media().wanted().into_iter().collect();
        if wanted.is_empty() {
            return;
        }
        let Ok(found) = deckcraft_engine::cmd::file::open_presentation(name, &bytes) else { return };
        let mut items = Vec::new();
        for item in &found.media {
            if item.data.is_empty() {
                continue;
            }
            let hash = sha256_hex(&item.data);
            if wanted.contains(&hash) {
                items.push((hash, item.clone()));
            }
        }
        if items.is_empty() {
            return;
        }
        let _ = self.show(app, |shim, deck| {
            let mut any = false;
            for (hash, item) in &items {
                any |= shim.media_arrived(deck, hash, item);
            }
            Ok(any)
        });
        // What is still missing is asked for again soon: the picture may not be in a saved version yet.
        self.media_backoff_ms = MEDIA_FIRST_MS;
        self.media_next_ms = now_ms() + MEDIA_FIRST_MS;
    }

    /// Once per frame BEFORE the app's own pass.
    pub fn pre(&mut self, _app: &mut SlideApp) {}

    /// Once per frame AFTER the app's own pass.
    pub fn post(&mut self, app: &mut SlideApp) {
        if !self.loaded && self.phase != Phase::Off {
            let now = app.host_doc.as_ref().map(|(uid, _)| *uid);
            if now.is_some() && now != self.baseline {
                self.loaded = true;
                self.doc_uid = now.unwrap_or(0);
                app.session.collab_undo = false;
            }
        }
        while self.loaded || matches!(self.held.front(), Some(Event::Refused { .. })) {
            match self.held.pop_front() {
                Some(Event::Refused { code, reason }) => app.set_status(if code.is_empty() { reason } else { format!("{reason} ({code})") }),
                Some(Event::Crdt { kind, body }) => self.handle(app, &kind, &body),
                Some(Event::MediaFile { name, version_id, bytes }) => self.media_file(app, &name, &version_id, bytes),
                Some(_) => {}
                None => break,
            }
        }
        if self.loaded {
            self.flush_local(app);
        }
        for undo in std::mem::take(&mut app.session.collab_undo_requests) {
            self.bridge.post_crdt(if undo { "undo" } else { "redo" }, &json!({}));
        }
        if self.phase == Phase::Live {
            self.share_cursor(app);
            self.show_peers(app);
            self.ask_for_pictures();
        }
    }

    /// Say where my selection is: when it moves, and now and then while it stays.
    fn share_cursor(&mut self, app: &SlideApp) {
        let now = now_ms();
        let Some(d) = app.session.active() else { return };
        let Some(slide) = d.current_slide() else { return };
        let Some(body) = self.shim.cursor_for(slide.id, &d.selection.shapes) else { return };
        let moved = self.last_cursor.as_ref() != Some(&body);
        let due = if moved {
            now.saturating_sub(self.last_cursor_ms) >= CURSOR_MIN_MS
        } else {
            now.saturating_sub(self.last_cursor_ms) >= CURSOR_REPEAT_MS
        };
        if !due {
            return;
        }
        self.bridge.post_crdt("cursor", &json!({"cursor": body}));
        self.last_cursor = Some(body);
        self.last_cursor_ms = now;
    }

    /// The other people's selections, resolved against the deck as it is now.
    fn show_peers(&mut self, app: &mut SlideApp) {
        let cursors: Vec<RemoteCursor> = self
            .peers
            .values()
            .filter_map(|p| {
                self.shim.peer_cursor(&p.cursor).map(|c| RemoteCursor { name: p.name.clone(), color: p.color, slide: c.slide, shapes: c.shapes })
            })
            .collect();
        if cursors != app.session.remote_cursors {
            app.session.remote_cursors = cursors;
        }
    }

    /// Pictures the shared deck uses and this window has no bytes for, and the picture this window inserted.
    fn ask_for_pictures(&mut self) {
        let now = now_ms();
        let wanted = self.shim.media().wanted();
        if wanted.is_empty() {
            self.media_backoff_ms = MEDIA_FIRST_MS;
            self.media_next_ms = 0;
        } else {
            if self.media_next_ms == 0 {
                self.media_next_ms = now + MEDIA_FIRST_MS;
            }
            if now >= self.media_next_ms {
                self.bridge.post_media_need(&wanted, &self.media_version);
                self.media_next_ms = now + self.media_backoff_ms;
                self.media_backoff_ms = (self.media_backoff_ms * 2).min(MEDIA_MAX_MS);
            }
        }
        if let Some((asked_ms, saved_then)) = self.publish_at {
            if self.bridge.saved_count() > saved_then {
                self.publish_at = None;
            } else if now.saturating_sub(asked_ms) >= PUBLISH_REPEAT_MS {
                self.bridge.post_media_publish();
                self.publish_at = Some((now, saved_then));
            }
        }
    }
}
