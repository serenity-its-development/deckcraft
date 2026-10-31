//! Slide comments through the host's comments tier ([`crate::comments`]): the translation between a slide's list of comments
//! (each with its replies) and the host's flat map of comments.
//!
//! A slide's comments are flattened to one entry each (a reply follows its parent). Each entry has a number `wc`, drawn at
//! random by the window that made the comment and carried in its anchor, so every window names it the same: an entry of the
//! deck is paired with the entry it was last time (unchanged ones at both ends, the changed ones in place), and a new one draws
//! a number. The host's map is the truth for comments it has named; a comment of this window the host has not named yet stays
//! where it is until it does.

use std::collections::{BTreeMap, BTreeSet};

use deckcraft_model::{Comment, Presentation, ShapeId, Slide};
use serde_json::{Value, json};

use crate::comments::{CommentMsg, LocalComment};
use crate::{Change, Shim, ShimError};

/// One comment as the sync holds it: the entry without its replies, and where its parent is in the flattened list.
pub type Flat = (Comment, Option<usize>);

/// A slide's comments, one entry each, a reply directly after its parent.
#[must_use]
pub fn flatten(comments: &[Comment]) -> Vec<Flat> {
    let mut out = Vec::new();
    for c in comments {
        let at = out.len();
        let mut top = c.clone();
        top.replies = Vec::new();
        out.push((top, None));
        for r in &c.replies {
            let mut reply = r.clone();
            reply.replies = Vec::new();
            out.push((reply, Some(at)));
        }
    }
    out
}

/// The inverse of [`flatten`].
#[must_use]
pub fn unflatten(flat: &[Flat]) -> Vec<Comment> {
    let mut out: Vec<Comment> = Vec::new();
    let mut top_of: BTreeMap<usize, usize> = BTreeMap::new();
    for (i, (c, parent)) in flat.iter().enumerate() {
        match parent.and_then(|p| top_of.get(&p).copied()) {
            Some(t) => out[t].replies.push(c.clone()),
            None => {
                top_of.insert(i, out.len());
                out.push(c.clone());
            }
        }
    }
    out
}

/// What the sync knows of a comment of the host's.
#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub slide: String,
    pub parent: Option<u32>,
    pub comment: Comment,
}

/// The same comment as far as the shared state is concerned (the author and date are the host's stamp).
fn same(a: &Flat, b: &Flat) -> bool {
    a.1 == b.1 && a.0.text == b.0.text && a.0.resolved == b.0.resolved && a.0.shape == b.0.shape
}

impl Shim {
    /// The comment messages the last local calls produced. Send each as `craft:crdt:comment`.
    pub fn take_comment_msgs(&mut self) -> Vec<CommentMsg> {
        std::mem::take(&mut self.comment_msgs)
    }

    /// Pair each slide's comments with the numbers they had, draw numbers for the new ones, and queue the messages that take the
    /// host's comments to the deck's.
    pub fn local_comments(&mut self, deck: &Presentation) {
        let mut all: BTreeMap<u32, LocalComment> = BTreeMap::new();
        let mut next_ids: std::collections::HashMap<String, Vec<(u32, Flat)>> = std::collections::HashMap::new();
        for slide in &deck.slides {
            let sk = self.ids.0.borrow_mut().slide_key(slide.id);
            let new = flatten(&slide.comments);
            let old = self.comment_ids.get(&sk).cloned().unwrap_or_default();
            let (mut pre, mut suf) = (0, 0);
            while pre < old.len() && pre < new.len() && same(&old[pre].1, &new[pre]) {
                pre += 1;
            }
            while suf < old.len() - pre && suf < new.len() - pre && same(&old[old.len() - 1 - suf].1, &new[new.len() - 1 - suf]) {
                suf += 1;
            }
            let (old_mid, new_mid) = (old.len() - pre - suf, new.len() - pre - suf);
            let mut numbered: Vec<(u32, Flat)> = Vec::with_capacity(new.len());
            for (i, flat) in new.iter().enumerate() {
                let wc = if i < pre {
                    old[i].0
                } else if i >= new.len() - suf {
                    old[old.len() - (new.len() - i)].0
                } else if i - pre < old_mid.min(new_mid) {
                    old[i].0
                } else {
                    self.ids.0.borrow_mut().mint_wc()
                };
                numbered.push((wc, flat.clone()));
            }
            for (i, (wc, (c, parent))) in numbered.iter().enumerate() {
                let anchor = {
                    let mut ids = self.ids.0.borrow_mut();
                    let mut a = json!({"slide": sk});
                    if parent.is_none() {
                        if let Some(shape) = c.shape {
                            a["shape"] = json!(ids.shape_key(shape));
                        }
                        a["x"] = json!(c.x);
                        a["y"] = json!(c.y);
                    }
                    a
                };
                let _ = i;
                all.insert(*wc, LocalComment { text: c.text.clone(), resolved: c.resolved, parent: parent.map(|p| numbered[p].0), anchor });
            }
            next_ids.insert(sk, numbered);
        }
        // A comment whose slide is gone is gone.
        self.comment_ids = next_ids;
        let msgs = self.comments.local(&all);
        self.comment_msgs.extend(msgs);
    }

    /// The host's comments arrived (`craft:crdt:state`'s `comments`).
    pub fn remote_comments(&mut self, host: &Value) {
        self.host_comments = host.clone();
        self.apply_host_comments();
    }

    /// A host delta's changes: those below `["comments"]` change the host's comments (the whole value, or one entry or field of
    /// it); the rest are not comments' and are ignored here.
    pub fn remote_comment_changes(&mut self, changes: &[Change]) {
        let mut touched = false;
        for c in changes {
            if c.path.first().map(String::as_str) != Some("comments") {
                continue;
            }
            touched = true;
            if c.path.len() == 1 {
                self.host_comments = c.value.clone().unwrap_or(Value::Null);
                continue;
            }
            if !self.host_comments.is_object() {
                self.host_comments = Value::Object(serde_json::Map::new());
            }
            set_nested(&mut self.host_comments, &c.path[1..], c.value.as_ref());
        }
        if touched {
            self.apply_host_comments();
        }
    }

    fn apply_host_comments(&mut self) {
        let host = self.host_comments.clone();
        // The protocol's own state (which numbers the host has named, deletes that waited for an id).
        let (_update, deletes) = self.comments.remote(&host);
        self.comment_msgs.extend(deletes);
        let first = self.slide_keys().into_iter().next();
        let now = self.known_from_host(&host, first);
        for (wc, k) in &now {
            if self.comment_known.get(wc) != Some(k) {
                self.comments_dirty.insert(k.slide.clone());
            }
        }
        for (wc, k) in &self.comment_known {
            if !now.contains_key(wc) {
                self.comments_dirty.insert(k.slide.clone());
            }
        }
        self.comment_seen.extend(now.keys().copied());
        self.comment_known = now;
    }

    /// Every comment the host holds, by number, with the slide it is on: its anchor's, else its parent's, else the first slide.
    fn known_from_host(&self, host: &Value, first: Option<String>) -> BTreeMap<u32, Known> {
        let mut out: BTreeMap<u32, Known> = BTreeMap::new();
        let Some(map) = host.as_object() else { return out };
        let mut wc_of: BTreeMap<&str, u32> = BTreeMap::new();
        for (id, c) in map {
            let wc = c
                .get("anchor")
                .and_then(|a| a.get("wc"))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or_else(|| crate::comments::derived_wc(id));
            wc_of.insert(id.as_str(), wc);
        }
        // Slides first for the comments that state one, then the replies that follow their parent.
        let mut slide_of: BTreeMap<u32, String> = BTreeMap::new();
        for (id, c) in map {
            if let Some(sk) = c.get("anchor").and_then(|a| a.get("slide")).and_then(Value::as_str) {
                slide_of.insert(wc_of[id.as_str()], sk.to_owned());
            }
        }
        for _ in 0..2 {
            for (id, c) in map {
                let wc = wc_of[id.as_str()];
                if slide_of.contains_key(&wc) {
                    continue;
                }
                if let Some(sk) = c.get("parent").and_then(Value::as_str).and_then(|p| wc_of.get(p)).and_then(|pw| slide_of.get(pw)).cloned() {
                    slide_of.insert(wc, sk);
                }
            }
        }
        for (id, c) in map {
            let wc = wc_of[id.as_str()];
            let Some(slide) = slide_of.get(&wc).cloned().or_else(|| first.clone()) else { continue };
            let anchor = c.get("anchor").cloned().unwrap_or(Value::Null);
            let name = c.get("name").and_then(Value::as_str).unwrap_or("Someone").to_owned();
            let at = c.get("at").and_then(Value::as_u64).unwrap_or(0);
            let comment = Comment {
                initials: crate::comments::initials_of(&name),
                author: name,
                text: c.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(),
                date: crate::comments::iso_of(at),
                x: anchor.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                y: anchor.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                resolved: c.get("resolved").and_then(Value::as_bool) == Some(true),
                replies: Vec::new(),
                shape: anchor.get("shape").and_then(Value::as_str).and_then(|k| self.ids.0.borrow().known_shape(k)),
            };
            let parent = c.get("parent").and_then(Value::as_str).and_then(|p| wc_of.get(p)).copied();
            out.insert(wc, Known { slide, parent, comment });
        }
        out
    }

    /// Put the host's comments on the slides whose comments changed. Answers whether the deck changed.
    pub fn apply_comments(&mut self, deck: &mut Presentation) -> Result<bool, ShimError> {
        let dirty = std::mem::take(&mut self.comments_dirty);
        let mut changed = false;
        for sk in dirty {
            let Some(id) = self.ids.0.borrow().known_slide(&sk) else { continue };
            let Some(at) = deck.slides.iter().position(|s| s.id == id) else { continue };
            // The comments of this slide: the host's, in the order they were made (a reply after its parent).
            let mut mine: Vec<(u32, &Known)> = self.comment_known.iter().filter(|(_, k)| k.slide == sk).map(|(w, k)| (*w, k)).collect();
            mine.sort_by(|a, b| (&a.1.comment.date, a.0).cmp(&(&b.1.comment.date, b.0)));
            let mut flat: Vec<(u32, Flat)> = Vec::new();
            let mut index: BTreeMap<u32, usize> = BTreeMap::new();
            for (wc, k) in mine.iter().filter(|(_, k)| k.parent.is_none()) {
                index.insert(*wc, flat.len());
                flat.push((*wc, (k.comment.clone(), None)));
                for (rw, r) in mine.iter().filter(|(_, r)| r.parent == Some(*wc)) {
                    flat.push((*rw, (r.comment.clone(), index.get(wc).copied())));
                }
            }
            // This window's own, not yet named by the host.
            let theirs: BTreeSet<u32> = self.comment_known.keys().copied().collect();
            if let Some(local) = self.comment_ids.get(&sk) {
                for (wc, (c, parent)) in local {
                    // Not named by the host yet (not: named once and gone since, which the host deleted).
                    if !theirs.contains(wc) && !self.comment_seen.contains(wc) {
                        let p = parent.and_then(|p| local.get(p)).and_then(|(pw, _)| flat.iter().position(|(w, _)| w == pw));
                        flat.push((*wc, (c.clone(), p)));
                    }
                }
            }
            let list = unflatten(&flat.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>());
            if deck.slides[at].comments != list {
                let mut s: Slide = (*deck.slides[at]).clone();
                s.comments = list;
                let s = std::sync::Arc::new(s);
                self.synced.insert(sk.clone(), s.clone());
                deck.slides[at] = s;
                changed = true;
            }
            self.comment_ids.insert(sk, flat);
        }
        Ok(changed)
    }

    /// The comment on `shape` of the deck (for tests and the cursor).
    #[must_use]
    pub fn comment_numbers(&self, slide: &str) -> Vec<u32> {
        self.comment_ids.get(slide).map(|v| v.iter().map(|(w, _)| *w).collect()).unwrap_or_default()
    }

    #[allow(dead_code)]
    fn shape_of(&self, id: ShapeId) -> String {
        self.shape_key(id)
    }
}

/// Set (or remove, with no value) the entry at `path` below the object `at`, making the objects on the way.
fn set_nested(at: &mut Value, path: &[String], value: Option<&Value>) {
    let Some((first, rest)) = path.split_first() else { return };
    if !at.is_object() {
        *at = Value::Object(serde_json::Map::new());
    }
    let Some(obj) = at.as_object_mut() else { return };
    if rest.is_empty() {
        match value {
            Some(v) => {
                obj.insert(first.clone(), v.clone());
            }
            None => {
                obj.remove(first);
            }
        }
    } else {
        set_nested(obj.entry(first.clone()).or_insert_with(|| Value::Object(serde_json::Map::new())), rest, value);
    }
}
