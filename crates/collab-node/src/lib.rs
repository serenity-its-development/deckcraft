//! The collaboration shim for node: one [`Window`] is one editor window's side of the host bridge, with the deck held as the
//! model, so a test (OneOS's `craft-slides-convergence.test.ts`) can drive two windows through the host's real adapter and CRDT
//! engine and compare what they end up showing. Everything the browser build shares is here: slides, shapes, text with
//! formatting, notes, comments, pictures (by content address) and cursors.

use std::sync::Arc;

use deckcraft_collab::comments::CommentMsg;
use deckcraft_collab::{Change, Shim, changes_of};
use deckcraft_model::{Comment, MediaId, MediaItem, Presentation, Run, RunProps, Shape, ShapeId, ShapeKind, Slide, SlideId, TextBody};
use serde_json::{Value, json};
use wasm_bindgen::prelude::*;

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn batches_json(batches: &[deckcraft_collab::Batch]) -> String {
    Value::Array(batches.iter().map(|b| json!({"seq": b.seq, "ops": b.ops, "label": b.label, "undo": if b.join { "join" } else { "step" }, "seed": b.seed})).collect())
        .to_string()
}

/// Edit a model value through its JSON: merge `patch` (an object) into it.
fn patched<T: serde::Serialize + serde::de::DeserializeOwned>(v: &T, patch: &str) -> Result<T, JsError> {
    let mut cur = serde_json::to_value(v).map_err(err)?;
    let patch: Value = serde_json::from_str(patch).map_err(err)?;
    if let (Some(c), Some(p)) = (cur.as_object_mut(), patch.as_object()) {
        for (k, x) in p {
            if x.is_null() {
                c.remove(k);
            } else {
                c.insert(k.clone(), x.clone());
            }
        }
    }
    serde_json::from_value(cur).map_err(err)
}

#[wasm_bindgen]
pub struct Window {
    shim: Shim,
    /// What the editor holds.
    deck: Presentation,
}

impl Window {
    fn slide_mut(&mut self, si: usize) -> Result<Slide, JsError> {
        self.deck.slides.get(si).map(|s| (**s).clone()).ok_or_else(|| JsError::new("no such slide"))
    }

    fn put(&mut self, si: usize, s: Slide) {
        self.deck.slides[si] = Arc::new(s);
    }

    /// A new id from the deck's own counter, as the engine's `alloc_id` does (the shim advances it for the ids it makes).
    fn alloc(&mut self) -> u32 {
        self.deck.next_id += 1;
        self.deck.next_id
    }
}

#[wasm_bindgen]
impl Window {
    /// A window showing the deck `spec_json` describes: `{slides: [{id, shapes: [{id, text}], notes?}]}`; the editor's next id
    /// is `first_id` (two windows pick different ones, as two editors would).
    #[wasm_bindgen(constructor)]
    pub fn new(seed: u32, spec_json: &str, first_id: u32) -> Result<Window, JsError> {
        let spec: Value = serde_json::from_str(spec_json).map_err(err)?;
        let mut deck = Presentation::default();
        deck.slides.clear();
        for s in spec.get("slides").and_then(Value::as_array).cloned().unwrap_or_default() {
            let mut slide = Slide { id: SlideId(s.get("id").and_then(Value::as_u64).unwrap_or(1) as u32), ..Default::default() };
            for sh in s.get("shapes").and_then(Value::as_array).cloned().unwrap_or_default() {
                slide.shapes.push(Shape {
                    id: ShapeId(sh.get("id").and_then(Value::as_u64).unwrap_or(1) as u32),
                    name: sh.get("name").and_then(Value::as_str).unwrap_or("Box").to_owned(),
                    text: sh.get("text").and_then(Value::as_str).map(TextBody::from_text),
                    ..Default::default()
                });
            }
            if let Some(n) = s.get("notes").and_then(Value::as_str) {
                slide.notes = TextBody::from_text(n);
            }
            deck.slides.push(Arc::new(slide));
        }
        deck.next_id = first_id;
        Ok(Window { shim: Shim::new(u64::from(seed) * 0x9e37_79b9 + 1), deck })
    }

    // ── the host side ─────────────────────────────────────────────────────────────────────────────────────────────

    /// The seed batches for the opened deck (an array of `{seq, ops, label, undo, seed}`).
    pub fn seed(&mut self, now: f64) -> Result<String, JsError> {
        let batches = self.shim.seed(&self.deck, now as u64).map_err(err)?;
        self.shim.local_comments(&self.deck);
        Ok(batches_json(&batches))
    }

    /// The person changed the deck: answers the batches to send (the comment messages are in [`Self::comment_msgs`]).
    pub fn flush(&mut self, now: f64) -> Result<String, JsError> {
        let batches = self.shim.local(&self.deck, now as u64).map_err(err)?;
        self.shim.local_comments(&self.deck);
        Ok(batches_json(&batches))
    }

    /// The host sent `craft:crdt:state`: `doc_json` is its `doc`, `comments_json` its `comments`.
    pub fn state(&mut self, doc_json: &str, comments_json: &str) -> Result<(), JsError> {
        let doc: Value = serde_json::from_str(doc_json).map_err(|e| JsError::new(&format!("state: {e}")))?;
        self.shim.remote_state(&doc, &mut self.deck).map_err(err)?;
        let comments: Value = serde_json::from_str(comments_json).unwrap_or(Value::Null);
        self.shim.remote_comments(&comments);
        self.shim.apply_comments(&mut self.deck).map_err(err)?;
        Ok(())
    }

    /// The host sent `craft:crdt:delta`: `changes_json` is its `changes` (paths below `doc` carry the leading `doc`; a change at
    /// `["comments"]` is the whole comments value); answers whether the shown deck changed.
    pub fn delta(&mut self, changes_json: &str, ack: f64, now: f64) -> Result<bool, JsError> {
        let all: Value = serde_json::from_str(changes_json).map_err(|e| JsError::new(&format!("changes: {e}")))?;
        let changes = changes_of(&all);
        self.shim.remote_comment_changes(&changes);
        let doc_changes: Vec<Change> = changes
            .into_iter()
            .filter_map(|mut c| {
                (c.path.first().map(String::as_str) == Some("doc")).then(|| {
                    c.path.remove(0);
                    c
                })
            })
            .collect();
        let mut changed = self.shim.remote_delta(&doc_changes, ack as u64, now as u64, &mut self.deck).map_err(err)?;
        changed |= self.shim.apply_comments(&mut self.deck).map_err(err)?;
        Ok(changed)
    }

    /// The comment messages the last local calls produced (an array of `craft:crdt:comment` bodies).
    pub fn comment_msgs(&mut self) -> String {
        Value::Array(self.shim.take_comment_msgs().iter().map(CommentMsg::body).collect()).to_string()
    }

    // ── what the editor shows ─────────────────────────────────────────────────────────────────────────────────────

    /// Everything shared the window shows, as one comparable JSON string (shapes by shared key, formatting per character).
    pub fn digest(&self) -> Result<String, JsError> {
        Ok(self.shim.digest(&self.deck).map_err(err)?.to_string())
    }

    /// A summary the tests read: `[{id, shapes: [{id, name, text}], notes, comments: [text]}]`.
    pub fn summary(&self) -> String {
        Value::Array(
            self.deck
                .slides
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id.0,
                        "shapes": s.shapes.iter().map(|x| json!({"id": x.id.0, "name": x.name, "text": x.text.as_ref().map(TextBody::text)})).collect::<Vec<_>>(),
                        "notes": s.notes.text(),
                        "comments": s.comments.iter().map(|c| json!({"text": c.text, "resolved": c.resolved, "replies": c.replies.iter().map(|r| r.text.clone()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
        .to_string()
    }

    /// The deck's slide ids in order.
    pub fn slide_ids(&self) -> String {
        json!(self.deck.slides.iter().map(|s| s.id.0).collect::<Vec<_>>()).to_string()
    }

    // ── the person's edits ────────────────────────────────────────────────────────────────────────────────────────

    /// Replace the text of a shape (paragraphs split at newlines, runs unformatted).
    pub fn set_text(&mut self, si: usize, xi: usize, text: &str) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        s.shapes.get_mut(xi).ok_or_else(|| JsError::new("no such shape"))?.text = Some(TextBody::from_text(text));
        self.put(si, s);
        Ok(())
    }

    /// Type `text` into paragraph `pi` of a shape at character `at`, in the formatting of the run there.
    pub fn type_at(&mut self, si: usize, xi: usize, pi: usize, at: usize, text: &str) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        let p = s.shapes.get_mut(xi).and_then(|x| x.text.as_mut()).and_then(|t| t.paragraphs.get_mut(pi)).ok_or_else(|| JsError::new("no such paragraph"))?;
        let mut off = 0;
        for run in &mut p.runs {
            let n = run.text.chars().count();
            if at <= off + n {
                let b = run.text.char_indices().nth(at - off).map_or(run.text.len(), |(b, _)| b);
                run.text.insert_str(b, text);
                self.put(si, s);
                return Ok(());
            }
            off += n;
        }
        p.runs.push(Run::new(text));
        self.put(si, s);
        Ok(())
    }

    /// Make the characters `start..end` of paragraph `pi` bold (or not), splitting runs.
    pub fn bold(&mut self, si: usize, xi: usize, pi: usize, start: usize, end: usize, on: bool) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        let p = s.shapes.get_mut(xi).and_then(|x| x.text.as_mut()).and_then(|t| t.paragraphs.get_mut(pi)).ok_or_else(|| JsError::new("no such paragraph"))?;
        let mut runs: Vec<Run> = Vec::new();
        let mut off = 0;
        for run in &p.runs {
            let chars: Vec<char> = run.text.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let at = off + i;
                let inside = at >= start && at < end;
                let mut j = i;
                while j < chars.len() && ((off + j >= start && off + j < end) == inside) {
                    j += 1;
                }
                let mut props = run.props.clone();
                if inside {
                    props.bold = Some(on);
                }
                runs.push(Run::with(chars[i..j].iter().collect::<String>(), props));
                i = j;
            }
            off += chars.len();
        }
        p.runs = runs;
        self.put(si, s);
        Ok(())
    }

    /// Set the size of every run of paragraph `pi`.
    pub fn size(&mut self, si: usize, xi: usize, pi: usize, size: f64) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        let p = s.shapes.get_mut(xi).and_then(|x| x.text.as_mut()).and_then(|t| t.paragraphs.get_mut(pi)).ok_or_else(|| JsError::new("no such paragraph"))?;
        for run in &mut p.runs {
            run.props = RunProps { size: Some(size), ..run.props.clone() };
        }
        self.put(si, s);
        Ok(())
    }

    /// Set the outline level of paragraph `pi`.
    pub fn level(&mut self, si: usize, xi: usize, pi: usize, level: u8) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        let p = s.shapes.get_mut(xi).and_then(|x| x.text.as_mut()).and_then(|t| t.paragraphs.get_mut(pi)).ok_or_else(|| JsError::new("no such paragraph"))?;
        p.level = level;
        self.put(si, s);
        Ok(())
    }

    /// Merge `patch_json` (an object of the shape's own JSON fields: `xfrm`, `fill`, `name`...) into a shape.
    pub fn patch_shape(&mut self, si: usize, xi: usize, patch_json: &str) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        let sh = s.shapes.get_mut(xi).ok_or_else(|| JsError::new("no such shape"))?;
        let text = sh.text.clone();
        let id = sh.id;
        let mut next: Shape = patched(sh, patch_json)?;
        next.id = id;
        next.text = text;
        *sh = next;
        self.put(si, s);
        Ok(())
    }

    /// Merge `patch_json` into a slide's own JSON fields (`name`, `hidden`, `background`...).
    pub fn patch_slide(&mut self, si: usize, patch_json: &str) -> Result<(), JsError> {
        let s = self.slide_mut(si)?;
        let next: Slide = patched(&s, patch_json)?;
        self.put(si, Slide { id: s.id, shapes: s.shapes, notes: s.notes, comments: s.comments, ..next });
        Ok(())
    }

    /// Add a shape with `text` at the end of a slide (or inside group `into`, an index in the slide). Answers its id.
    pub fn add_shape(&mut self, si: usize, text: &str, into: i32) -> Result<u32, JsError> {
        let id = self.alloc();
        let mut s = self.slide_mut(si)?;
        let shape = Shape { id: ShapeId(id), name: format!("Shape {id}"), text: Some(TextBody::from_text(text)), ..Default::default() };
        if into >= 0 {
            match s.shapes.get_mut(into as usize).map(|g| &mut g.kind) {
                Some(ShapeKind::Group { children, .. }) => children.push(shape),
                _ => return Err(JsError::new("not a group")),
            }
        } else {
            s.shapes.push(shape);
        }
        self.put(si, s);
        Ok(id)
    }

    /// Add an empty group at the end of a slide. Answers its id.
    pub fn add_group(&mut self, si: usize) -> Result<u32, JsError> {
        let id = self.alloc();
        let mut s = self.slide_mut(si)?;
        s.shapes.push(Shape { id: ShapeId(id), name: format!("Group {id}"), kind: ShapeKind::Group { children: vec![], child: Default::default() }, ..Default::default() });
        self.put(si, s);
        Ok(id)
    }

    /// Add a connector between two shapes (by index in the slide). Answers its id.
    pub fn add_connector(&mut self, si: usize, from: usize, to: usize) -> Result<u32, JsError> {
        let id = self.alloc();
        let mut s = self.slide_mut(si)?;
        let (a, b) = (s.shapes.get(from).map(|x| x.id), s.shapes.get(to).map(|x| x.id));
        s.shapes.push(Shape { id: ShapeId(id), kind: ShapeKind::Connector { start: a.map(|i| (i, 1)), end: b.map(|i| (i, 2)) }, ..Default::default() });
        self.put(si, s);
        Ok(id)
    }

    pub fn remove_shape(&mut self, si: usize, xi: usize) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        if xi >= s.shapes.len() {
            return Err(JsError::new("no such shape"));
        }
        s.shapes.remove(xi);
        self.put(si, s);
        Ok(())
    }

    /// Move a shape within its slide's z-order.
    pub fn reorder_shape(&mut self, si: usize, from: usize, to: usize) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        if from >= s.shapes.len() || to >= s.shapes.len() {
            return Err(JsError::new("no such shape"));
        }
        let x = s.shapes.remove(from);
        s.shapes.insert(to, x);
        self.put(si, s);
        Ok(())
    }

    /// Add a slide with one text shape at index `at`. Answers the slide's id.
    pub fn add_slide(&mut self, at: usize, text: &str) -> u32 {
        let id = self.alloc();
        let sh = self.alloc();
        let slide = Slide { id: SlideId(id), shapes: vec![Shape { id: ShapeId(sh), name: format!("Shape {sh}"), text: Some(TextBody::from_text(text)), ..Default::default() }], ..Default::default() };
        self.deck.slides.insert(at.min(self.deck.slides.len()), Arc::new(slide));
        id
    }

    pub fn remove_slide(&mut self, si: usize) -> Result<(), JsError> {
        if si >= self.deck.slides.len() {
            return Err(JsError::new("no such slide"));
        }
        self.deck.slides.remove(si);
        Ok(())
    }

    pub fn move_slide(&mut self, from: usize, to: usize) -> Result<(), JsError> {
        if from >= self.deck.slides.len() || to >= self.deck.slides.len() {
            return Err(JsError::new("no such slide"));
        }
        let s = self.deck.slides.remove(from);
        self.deck.slides.insert(to, s);
        Ok(())
    }

    pub fn set_notes(&mut self, si: usize, text: &str) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        s.notes = TextBody::from_text(text);
        self.put(si, s);
        Ok(())
    }

    /// Add a picture shape on a slide whose bytes are `bytes` (named by content). Answers the shape's id.
    pub fn add_picture(&mut self, si: usize, name: &str, bytes: &[u8]) -> Result<u32, JsError> {
        let media = MediaId(self.alloc());
        self.deck.media.push(MediaItem { id: media, name: name.to_owned(), content_type: "image/png".into(), data: Arc::new(bytes.to_vec()), link: None });
        let id = self.alloc();
        let mut s = self.slide_mut(si)?;
        s.shapes.push(Shape {
            id: ShapeId(id),
            name: name.to_owned(),
            kind: ShapeKind::Picture { fill: deckcraft_model::style::PictureFill { media, ..Default::default() } },
            ..Default::default()
        });
        self.put(si, s);
        Ok(id)
    }

    /// The pictures the shared deck uses that this window has no bytes for (an array of content addresses).
    pub fn wanted(&mut self) -> String {
        serde_json::to_string(&self.shim.take_shown().wanted).unwrap_or_default()
    }

    /// The bytes of the picture `hash` names were found: fill the placeholder. Answers whether they were taken.
    pub fn store_media(&mut self, hash: &str, bytes: &[u8]) -> bool {
        let item = MediaItem { id: MediaId(0), name: "found.png".into(), content_type: "image/png".into(), data: Arc::new(bytes.to_vec()), link: None };
        self.shim.media_arrived(&mut self.deck, hash, &item)
    }

    /// The picture bytes in the deck, by length: `{id: size}` for every item that has bytes.
    pub fn media_json(&self) -> String {
        Value::Object(self.deck.media.iter().filter(|m| !m.data.is_empty()).map(|m| (m.id.0.to_string(), json!(m.data.len()))).collect()).to_string()
    }

    // ── comments ──────────────────────────────────────────────────────────────────────────────────────────────────

    /// Add a comment on slide `si`; a reply (`reply_to` >= 0) goes under that top-level comment of the slide.
    pub fn add_comment(&mut self, si: usize, text: &str, reply_to: i32) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        let c = Comment { author: "Test".into(), initials: "T".into(), text: text.to_owned(), date: "2026-10-26T00:00:00Z".into(), ..Default::default() };
        if reply_to >= 0 {
            s.comments.get_mut(reply_to as usize).ok_or_else(|| JsError::new("no such comment"))?.replies.push(c);
        } else {
            s.comments.push(c);
        }
        self.put(si, s);
        Ok(())
    }

    pub fn resolve_comment(&mut self, si: usize, ci: usize, resolved: bool) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        s.comments.get_mut(ci).ok_or_else(|| JsError::new("no such comment"))?.resolved = resolved;
        self.put(si, s);
        Ok(())
    }

    pub fn edit_comment(&mut self, si: usize, ci: usize, text: &str) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        s.comments.get_mut(ci).ok_or_else(|| JsError::new("no such comment"))?.text = text.to_owned();
        self.put(si, s);
        Ok(())
    }

    pub fn delete_comment(&mut self, si: usize, ci: usize) -> Result<(), JsError> {
        let mut s = self.slide_mut(si)?;
        if ci >= s.comments.len() {
            return Err(JsError::new("no such comment"));
        }
        s.comments.remove(ci);
        self.put(si, s);
        Ok(())
    }

    // ── cursors ───────────────────────────────────────────────────────────────────────────────────────────────────

    /// The `craft:crdt:cursor` body for the slide `si` with the shapes `xis` (indexes) selected.
    pub fn cursor(&self, si: usize, xis: &str) -> String {
        let Some(slide) = self.deck.slides.get(si) else { return String::new() };
        let picked: Vec<usize> = serde_json::from_str(xis).unwrap_or_default();
        let ids: Vec<ShapeId> = picked.iter().filter_map(|i| slide.shapes.get(*i).map(|x| x.id)).collect();
        self.shim.cursor_for(slide.id, &ids).map(|v| v.to_string()).unwrap_or_default()
    }

    /// Where another person is, in this window: `{slide: index, shapes: [index]}`, or an empty string.
    pub fn peer(&self, cursor_json: &str) -> String {
        let Ok(v) = serde_json::from_str::<Value>(cursor_json) else { return String::new() };
        let Some(c) = self.shim.peer_cursor(&v) else { return String::new() };
        let Some(si) = self.deck.slides.iter().position(|s| s.id == c.slide) else { return String::new() };
        let shapes: Vec<usize> = c.shapes.iter().filter_map(|id| self.deck.slides[si].shapes.iter().position(|x| x.id == *id)).collect();
        json!({"slide": si, "shapes": shapes}).to_string()
    }
}

/// A deck spec of `n` slides each with a title and a body shape, ids counting up from `first`, for the tests.
#[wasm_bindgen]
pub fn sample_spec(n: u32, first: u32) -> String {
    let mut id = first;
    let mut slides = Vec::new();
    for i in 0..n {
        let sid = id;
        let t = id + 1;
        let b = id + 2;
        id += 3;
        slides.push(json!({"id": sid, "shapes": [{"id": t, "name": "Title", "text": format!("Slide {}", i + 1)}, {"id": b, "name": "Body", "text": "First point\nSecond point"}], "notes": format!("Notes {}", i + 1)}));
    }
    json!({"slides": slides}).to_string()
}
