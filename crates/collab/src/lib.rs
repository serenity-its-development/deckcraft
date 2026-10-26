//! DeckCraft's collaboration shim for OneOS (the "Slides" editor).
//!
//! The host (OneOS) keeps ONE replica of the file's shared document and speaks to the editor frame in the host bridge's
//! operation contract (`plans/mona/craft-integration-design-2026-10-25.md` section 12.3). This crate is the editor's side of
//! it, pure and testable without a browser:
//!
//! * [`Shim::seed`] and [`Shim::local`] turn the editor's deck into ops (`new`, `ensure`, `set`, `remove`, `text.insert`,
//!   `text.delete`, `text.mark`, `list.insert`, `list.delete`);
//! * [`Shim::remote_state`] and [`Shim::remote_delta`] turn what the host sends back (`craft:crdt:state`, `craft:crdt:delta`)
//!   into the slides to show.
//!
//! ★ WHAT IS SHARED, AND HOW (the schema beneath the doc's `craft` root). Everything an object holds beside its text is a set of
//! `f:<field>` properties, so two people changing different properties of one shape (its fill and its position) merge. Objects
//! are flat maps addressed by a KEY, with lists of keys for order, so a slide moved in the sorter or a shape sent backward keeps
//! its identity (and whatever others are typing in it).
//!
//! | what | where | how |
//! |---|---|---|
//! | slide order | `slides` | list of slide keys |
//! | a slide | `slide.<sk>` | `f:*` (layout, name, background, transition, animations, hidden...), `shapes` (list of shape keys), `nb`/`np` (the speaker notes' body properties and paragraph keys) |
//! | a shape | `shape.<xk>` | `f:*` (geometry, fill, line, kind...), `tb`/`tp` (text body properties, paragraph keys), `ch` (a group's children) |
//! | a paragraph | `para.<pk>` | `text` (CRDT text, a mark per run property `c:<field>`; a line break, field or equation is U+FFFC with an `obj` mark), `pv` (level and paragraph properties) |
//! | comments | the host's comments tier | add, reply, edit, resolve, reopen, delete messages ([`comments`]) |
//! | pictures | inside any `MediaId` | the picture's content address `cas-<sha256>`; the bytes are never in the room ([`media`]) |
//! | cursors | the room's awareness | [`Shim::cursor_for`] out, [`Shim::peer_cursor`] in |
//!
//! Shapes, slides and pictures are named by shared keys and content addresses wherever the model holds an id (the model's
//! `idcodec`), so connectors, animations and comments that name a shape keep naming it in every window.
//!
//! Not shared live: masters, layouts, the theme, slide size, sections, custom shows. They are the file's, as opened; a change
//! to them reaches other windows when the file is saved.
//!
//! The frame's own edits are not echoed back by the host, so the shim keeps its view of the shared tree itself: the host's
//! truth (`confirmed`) plus the local batches the host has not yet acknowledged (`pending`), re-applied over every delta (the
//! contract's client-side prediction). A batch counts as acknowledged when a message carries `ack >= seq`, or after
//! [`PENDING_MS`].

pub mod comments;
pub mod ids;
pub mod media;
pub mod mirror;
pub mod slide_comments;
pub mod sync;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use deckcraft_model::text::RunKind;
use deckcraft_model::{Paragraph, Presentation, Run, RunProps, ShapeId, ShapeKind, Slide, SlideId, TextBody};
use serde_json::{Map, Value, json};

use comments::{CommentMsg, CommentSync};
use ids::Ids;
use media::MediaIndex;
use mirror::{Marks, Mirror, MirrorError};
use sync::{FIELD, OBJ_CHAR, OBJECT, P, Text, canon, ensure_map, list_ids, node, node_in, normalize, path_of, plain, put_fields, put_list, put_text, put_value, read_fields, read_value};

/// A local batch is taken as applied by the host this long after it was sent, if no message acknowledged it sooner.
pub const PENDING_MS: u64 = 5_000;
/// At most this many local batches wait for an acknowledgement.
pub const MAX_PENDING: usize = 500;
/// The most ops one batch carries (the host's limit is 2000).
pub const BATCH_OPS: usize = 1_500;
/// Group nesting the shim follows (the host's path limit is 8 segments; groups nest by lists of keys, not by path).
const MAX_DEPTH: usize = 12;

/// Why the shim could not do what was asked. It is loud: the frame says it and falls back to single-editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShimError {
    /// A piece of the deck is bigger than one shared value holds.
    TooLarge(String),
    /// The shared tree holds something the model does not accept.
    Model(String),
    /// The shared tree and the shim disagree (an op did not apply).
    Mirror(String),
}

impl std::fmt::Display for ShimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShimError::TooLarge(m) | ShimError::Model(m) | ShimError::Mirror(m) => f.write_str(m),
        }
    }
}

impl From<MirrorError> for ShimError {
    fn from(e: MirrorError) -> Self {
        ShimError::Mirror(e.0)
    }
}

/// One batch of ops for `craft:crdt:ops`.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub seq: u64,
    pub ops: Vec<Value>,
    /// The app's own name for the edit (its caption in the room's history).
    pub label: String,
    /// Fold into the previous undo step (a typing burst).
    pub join: bool,
    /// The first batch of a seed (`seed: true`).
    pub seed: bool,
}

/// A change the host reports: set the tree at `path` (below `doc`) to `value`, or remove it.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub path: Vec<String>,
    pub value: Option<Value>,
}

/// A host message's `changes` array.
#[must_use]
pub fn changes_of(v: &Value) -> Vec<Change> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let path: Vec<String> = c.get("path")?.as_array()?.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect();
                    let removed = c.get("removed").and_then(Value::as_bool) == Some(true);
                    Some(Change { path, value: if removed { None } else { c.get("value").cloned() } })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Where another person is, resolved against this window's deck.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerCursor {
    pub slide: SlideId,
    pub shapes: Vec<ShapeId>,
}

/// What remote changes altered beyond the slides themselves.
#[derive(Debug, Clone, Default)]
pub struct Shown {
    /// Content addresses of pictures the shared deck uses that this window has no bytes for.
    pub wanted: Vec<String>,
}

fn json_of<T: serde::Serialize>(v: &T) -> Result<Value, ShimError> {
    serde_json::to_value(v).map_err(|e| ShimError::Model(e.to_string()))
}

fn model<T: serde::de::DeserializeOwned>(what: &str, mut v: Value) -> Result<T, ShimError> {
    normalize(&mut v);
    serde_json::from_value(v).map_err(|e| ShimError::Model(format!("{what}: {e}")))
}

// ── paragraphs ──────────────────────────────────────────────────────────────────────────────────────────────────────────

/// A paragraph as the shared tree holds it: its text with marks, and its paragraph-level properties.
#[derive(Debug, Clone, PartialEq)]
struct ParaD {
    text: Text,
    pv: Value,
}

fn field_marks(props: &RunProps) -> Result<Marks, ShimError> {
    let mut out = Marks::new();
    if let Value::Object(fields) = json_of(props)? {
        for (k, v) in fields {
            if !v.is_null() {
                out.insert(format!("{FIELD}{k}"), canon(&v));
            }
        }
    }
    Ok(out)
}

fn para_d(p: &Paragraph) -> Result<ParaD, ShimError> {
    let mut text = Text::default();
    for run in &p.runs {
        match &run.kind {
            RunKind::Text => {
                let marks = field_marks(&run.props)?;
                for c in run.text.chars() {
                    text.push(if c == OBJ_CHAR { '\u{FFFD}' } else { c }, &marks);
                }
            }
            _ => {
                let mut marks = Marks::new();
                marks.insert(OBJECT.to_owned(), canon(&json_of(run)?));
                text.push(OBJ_CHAR, &marks);
            }
        }
    }
    let mut pv = json_of(p)?;
    if let Some(o) = pv.as_object_mut() {
        o.remove("runs");
    }
    Ok(ParaD { text, pv: canon(&pv) })
}

fn run_props(marks: &Marks) -> Result<RunProps, ShimError> {
    let fields: Map<String, Value> = marks.iter().filter_map(|(k, v)| k.strip_prefix(FIELD).map(|f| (f.to_owned(), v.clone()))).collect();
    model("character formatting", Value::Object(fields))
}

fn para_from(d: &ParaD) -> Result<Paragraph, ShimError> {
    let mut runs: Vec<Run> = Vec::new();
    let mut open: Option<(Marks, String)> = None;
    let flush = |open: &mut Option<(Marks, String)>, runs: &mut Vec<Run>| -> Result<(), ShimError> {
        if let Some((marks, text)) = open.take() {
            runs.push(Run::with(text, run_props(&marks)?));
        }
        Ok(())
    };
    for (c, marks) in d.text.chars.iter().zip(&d.text.marks) {
        if *c == OBJ_CHAR {
            // An object character whose object did not arrive is no character at all.
            let Some(o) = marks.get(OBJECT) else { continue };
            flush(&mut open, &mut runs)?;
            runs.push(model("inline object", o.clone())?);
            continue;
        }
        let fields: Marks = marks.iter().filter(|(k, _)| k.starts_with(FIELD)).map(|(k, v)| (k.clone(), v.clone())).collect();
        match &mut open {
            Some((have, text)) if *have == fields => text.push(*c),
            _ => {
                flush(&mut open, &mut runs)?;
                open = Some((fields, c.to_string()));
            }
        }
    }
    flush(&mut open, &mut runs)?;
    let mut p: Paragraph = model("paragraph", d.pv.clone())?;
    p.runs = runs;
    Ok(p)
}

// ── objects as properties ───────────────────────────────────────────────────────────────────────────────────────────────

fn object_fields(v: Value, drop: &[&str]) -> Map<String, Value> {
    let Value::Object(o) = v else { return Map::new() };
    o.into_iter().filter(|(k, v)| !v.is_null() && !drop.contains(&k.as_str())).collect()
}

/// A slide's properties beside its shapes, notes and comments.
fn slide_fields(s: &Slide) -> Result<Map<String, Value>, ShimError> {
    Ok(object_fields(json_of(s)?, &["id", "shapes", "notes", "comments"]))
}

/// A shape's properties beside its text and a group's children.
fn shape_fields(s: &deckcraft_model::Shape) -> Result<Map<String, Value>, ShimError> {
    let mut v = json_of(s)?;
    if let Some(kind) = v.get_mut("kind").and_then(Value::as_object_mut)
        && kind.get("type").and_then(Value::as_str) == Some("group")
    {
        kind.remove("children");
    }
    Ok(object_fields(v, &["id", "text"]))
}

fn body_props(b: &TextBody) -> Result<Value, ShimError> {
    let mut v = json_of(b)?;
    if let Some(o) = v.as_object_mut() {
        o.remove("paragraphs");
    }
    Ok(v)
}

// ── the shim ───────────────────────────────────────────────────────────────────────────────────────────────────────────

pub struct Shim {
    ids: Ids,
    /// The shared tree as this window shows it (the host's truth plus the batches not yet acknowledged).
    view: Mirror,
    /// The host's truth, kept only while batches are pending.
    confirmed: Option<Mirror>,
    pending: Vec<(u64, u64, Vec<Value>)>,
    seq: u64,
    last_typing_ms: Option<u64>,
    /// Each slide as last synced, by key: an unchanged slide is not looked at again.
    synced: HashMap<String, Arc<Slide>>,
    shown: Shown,
    comments: CommentSync,
    comment_msgs: Vec<CommentMsg>,
    /// The host's comments as last told (the whole value; a delta changes a part of it).
    host_comments: Value,
    /// Each slide's comments as last synced: the number of each entry and the entry itself.
    comment_ids: HashMap<String, Vec<(u32, slide_comments::Flat)>>,
    /// The comments the host holds, by number.
    comment_known: BTreeMap<u32, slide_comments::Known>,
    /// Slides whose comments the host's changed.
    comments_dirty: BTreeSet<String>,
    /// Every comment number the host has ever named: one of this window's that is not in it is still on its way to the host.
    comment_seen: BTreeSet<u32>,
}

/// The keys of the shapes and paragraphs a slide owns in the shared tree.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Owned {
    shapes: BTreeSet<String>,
    paras: BTreeSet<String>,
}

/// Collect what the slide `sk` owns in `view`.
fn owned_by(view: &Mirror, sk: &str) -> Owned {
    let mut out = Owned::default();
    let sp = path_of(&["slide", sk]);
    owned_body(view, &sp, "np", &mut out);
    owned_shapes(view, &sp, "shapes", &mut out, 0);
    out
}

fn owned_body(view: &Mirror, path: &P, tp: &str, out: &mut Owned) {
    for pk in list_ids(view, path, tp).unwrap_or_default() {
        out.paras.insert(pk);
    }
}

fn owned_shapes(view: &Mirror, path: &P, list: &str, out: &mut Owned, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    for xk in list_ids(view, path, list).unwrap_or_default() {
        if !out.shapes.insert(xk.clone()) {
            continue;
        }
        let xp = path_of(&["shape", &xk]);
        owned_body(view, &xp, "tp", out);
        owned_shapes(view, &xp, "ch", out, depth + 1);
    }
}

/// What a local pass produced.
struct Pass {
    ops: Vec<Value>,
    owned: Owned,
}

struct Syncer<'a> {
    ids: &'a Ids,
    view: &'a Mirror,
    ops: &'a mut Vec<Value>,
    owned: Owned,
}

impl Syncer<'_> {
    fn slide(&mut self, slide: &Slide) -> Result<String, ShimError> {
        let sk = self.ids.0.borrow_mut().slide_key(slide.id);
        let sp = path_of(&["slide", &sk]);
        ensure_map(self.ops, self.view, &path_of(&["slide"]), &sk);
        put_fields(self.ops, self.view, &sp, &slide_fields(slide)?, "a slide")?;
        let keys = self.shapes(&sp, "shapes", &slide.shapes, 0)?;
        let _ = keys;
        self.body(&sp, "nb", "np", Some(&slide.notes))?;
        Ok(sk)
    }

    fn shapes(&mut self, parent: &P, list: &str, shapes: &[deckcraft_model::Shape], depth: usize) -> Result<Vec<String>, ShimError> {
        if depth > MAX_DEPTH {
            return Err(ShimError::Model("groups are nested too deeply to share".into()));
        }
        let mut keys = Vec::with_capacity(shapes.len());
        for sh in shapes {
            let xk = self.ids.0.borrow_mut().shape_key(sh.id);
            self.owned.shapes.insert(xk.clone());
            let xp = path_of(&["shape", &xk]);
            ensure_map(self.ops, self.view, &path_of(&["shape"]), &xk);
            put_fields(self.ops, self.view, &xp, &shape_fields(sh)?, "a shape")?;
            self.body(&xp, "tb", "tp", sh.text.as_ref())?;
            match &sh.kind {
                ShapeKind::Group { children, .. } => {
                    self.shapes(&xp, "ch", children, depth + 1)?;
                }
                _ => {
                    if node_in(self.view, &xp, "ch").is_some() {
                        self.ops.push(sync::op_remove(&xp, "ch"));
                    }
                }
            }
            keys.push(xk);
        }
        put_list(self.ops, self.view, parent, list, &keys);
        Ok(keys)
    }

    /// A text body at `path`: its properties (`tbk`) and its paragraphs (`tpk`, a list of paragraph keys).
    fn body(&mut self, path: &P, tbk: &str, tpk: &str, body: Option<&TextBody>) -> Result<(), ShimError> {
        let Some(body) = body else {
            for k in [tbk, tpk] {
                if node_in(self.view, path, k).is_some() {
                    self.ops.push(sync::op_remove(path, k));
                }
            }
            return Ok(());
        };
        put_value(self.ops, self.view, path, tbk, Some(&body_props(body)?), "a text body")?;
        let old_keys = list_ids(self.view, path, tpk).unwrap_or_default();
        let old: Vec<ParaD> = old_keys.iter().map(|k| self.view_para(k)).collect();
        let new: Vec<ParaD> = body.paragraphs.iter().map(para_d).collect::<Result<_, _>>()?;
        // Pair the old paragraphs with the new: the unchanged ones at both ends, then the changed ones in place.
        let mut pre = 0;
        while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
            pre += 1;
        }
        let mut suf = 0;
        while suf < old.len() - pre && suf < new.len() - pre && old[old.len() - 1 - suf] == new[new.len() - 1 - suf] {
            suf += 1;
        }
        let (old_mid, new_mid) = (old.len() - pre - suf, new.len() - pre - suf);
        let paired = old_mid.min(new_mid);
        let mut keys: Vec<String> = Vec::with_capacity(new.len());
        keys.extend(old_keys[..pre].iter().cloned());
        for k in 0..new_mid {
            let at = pre + k;
            if k < paired {
                let key = old_keys[at].clone();
                self.edit_para(&key, &new[at])?;
                keys.push(key);
            } else {
                let key = self.ids.0.borrow_mut().mint_key('p');
                self.create_para(&key, &new[at])?;
                keys.push(key);
            }
        }
        keys.extend(old_keys[old_keys.len() - suf..].iter().cloned());
        for k in &keys {
            self.owned.paras.insert(k.clone());
        }
        put_list(self.ops, self.view, path, tpk, &keys);
        Ok(())
    }

    fn view_para(&self, key: &str) -> ParaD {
        let pp = path_of(&["para", key]);
        ParaD {
            text: sync::view_text(self.view, &pp, "text").unwrap_or_default(),
            pv: canon(&read_value(self.view, &pp, "pv").unwrap_or_else(|| json!({}))),
        }
    }

    fn create_para(&mut self, key: &str, d: &ParaD) -> Result<(), ShimError> {
        let pp = path_of(&["para", key]);
        self.ops.push(sync::op_new(&path_of(&["para"]), key, "map"));
        put_text(self.ops, self.view, &pp, "text", &d.text)?;
        self.put_pv(&pp, d)
    }

    fn edit_para(&mut self, key: &str, d: &ParaD) -> Result<(), ShimError> {
        let pp = path_of(&["para", key]);
        if node(self.view, &pp).is_none() {
            return self.create_para(key, d);
        }
        put_text(self.ops, self.view, &pp, "text", &d.text)?;
        self.put_pv(&pp, d)
    }

    fn put_pv(&mut self, pp: &P, d: &ParaD) -> Result<(), ShimError> {
        let empty = d.pv.as_object().is_none_or(|o| o.is_empty());
        put_value(self.ops, self.view, pp, "pv", if empty { None } else { Some(&d.pv) }, "paragraph formatting")
    }
}

impl Shim {
    /// `seed`: random bits for keys (the caller draws them from the platform's randomness).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Shim {
            ids: Ids::new(seed),
            view: Mirror::default(),
            confirmed: None,
            pending: Vec::new(),
            seq: 0,
            last_typing_ms: None,
            synced: HashMap::new(),
            shown: Shown::default(),
            comments: CommentSync::default(),
            comment_msgs: Vec::new(),
            host_comments: Value::Null,
            comment_ids: HashMap::new(),
            comment_known: BTreeMap::new(),
            comments_dirty: BTreeSet::new(),
            comment_seen: BTreeSet::new(),
        }
    }

    /// The shared tree as this window shows it.
    #[must_use]
    pub fn view(&self) -> &Value {
        &self.view.root
    }

    /// The pictures this window knows (read-only).
    #[must_use]
    pub fn media(&self) -> std::cell::Ref<'_, MediaIndex> {
        std::cell::Ref::map(self.ids.0.borrow(), |i| &i.media)
    }

    fn push(&mut self, ops: Vec<Value>, label: &str, join: bool, seed: bool, now_ms: u64) -> Result<Batch, ShimError> {
        if self.pending.is_empty() {
            self.confirmed = Some(self.view.clone());
        }
        for op in &ops {
            self.view.apply(op)?;
        }
        self.seq += 1;
        self.pending.push((self.seq, now_ms, ops.clone()));
        self.prune(now_ms);
        Ok(Batch { seq: self.seq, ops, label: label.to_owned(), join, seed })
    }

    /// Send `ops` as one or more batches (the host takes at most 2000 ops a batch; every prefix of the ops leaves a valid tree).
    fn emit(&mut self, ops: Vec<Value>, label: &str, join: bool, seed: bool, now_ms: u64, out: &mut Vec<Batch>) -> Result<(), ShimError> {
        let mut rest = ops;
        let mut first = true;
        while !rest.is_empty() {
            let take: Vec<Value> = rest.drain(..BATCH_OPS.min(rest.len())).collect();
            let (name, joined, is_seed) = if first { (label.to_owned(), join, seed) } else { (format!("{label} (more)"), false, false) };
            out.push(self.push(take, &name, joined, is_seed, now_ms)?);
            first = false;
        }
        Ok(())
    }

    /// Fold old and surplus pending batches into the host's truth (they were applied).
    fn prune(&mut self, now_ms: u64) {
        while let Some((_, sent, _)) = self.pending.first() {
            if self.pending.len() <= MAX_PENDING && now_ms.saturating_sub(*sent) < PENDING_MS {
                break;
            }
            let (_, _, ops) = self.pending.remove(0);
            if let Some(c) = self.confirmed.as_mut() {
                for op in &ops {
                    // An op that no longer applies was refused by the host too; the next state message resets us.
                    let _ = c.apply(op);
                }
            }
        }
        if self.pending.is_empty() {
            self.confirmed = None;
        }
    }

    fn sync_ids_in(&self, deck: &Presentation) {
        let mut i = self.ids.0.borrow_mut();
        i.next_id = i.next_id.max(deck.next_id);
        i.media.refresh(&deck.media);
    }

    // ── seed and local edits ──────────────────────────────────────────────────────────────────────────────────────────

    /// ★ SEED: the whole deck as ops. The first batch is the seed (`seed: true`, one untracked change on the host); the rest
    /// follow as ordinary batches.
    pub fn seed(&mut self, deck: &Presentation, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.view = Mirror::default();
        self.confirmed = None;
        self.pending.clear();
        self.synced.clear();
        self.comments = CommentSync::default();
        self.comment_msgs.clear();
        self.comment_ids.clear();
        self.comment_known.clear();
        self.comments_dirty.clear();
        self.comment_seen.clear();
        self.sync_ids_in(deck);
        let mut ops = vec![
            json!({"op": "ensure", "path": [], "key": "slides", "kind": "list"}),
            json!({"op": "ensure", "path": [], "key": "slide", "kind": "map"}),
            json!({"op": "ensure", "path": [], "key": "shape", "kind": "map"}),
            json!({"op": "ensure", "path": [], "key": "para", "kind": "map"}),
        ];
        // The roots exist in the view before the diff looks at it.
        for op in &ops {
            self.view.apply(op)?;
        }
        let pass = Self::diff(&self.ids, &self.view, &mut self.synced, deck, true)?;
        ops.extend(pass.ops);
        let mut batches = Vec::new();
        self.emit(ops, "Opened the file", false, true, now_ms, &mut batches)?;
        Ok(batches)
    }

    /// The ops that take the view to `deck`, for the slides that changed since they were last synced.
    fn diff(ids: &Ids, view: &Mirror, synced: &mut HashMap<String, Arc<Slide>>, deck: &Presentation, all: bool) -> Result<Pass, ShimError> {
        let ids = ids.clone();
        let mut ops = Vec::new();
        let mut owned_now = Owned::default();
        let mut wanted_slides: Vec<String> = Vec::new();
        let mut changed: Vec<(String, Arc<Slide>)> = Vec::new();
        ids.scope(|| -> Result<(), ShimError> {
            for slide in &deck.slides {
                let sk = ids.0.borrow_mut().slide_key(slide.id);
                wanted_slides.push(sk.clone());
                let same = !all && synced.get(&sk).is_some_and(|old| Arc::ptr_eq(old, slide) || **old == **slide);
                if same {
                    continue;
                }
                let mut s = Syncer { ids: &ids, view, ops: &mut ops, owned: Owned::default() };
                s.slide(slide)?;
                let owned = s.owned;
                // What this slide used to own in the shared tree and does not any more is removed.
                let before = owned_by(view, &sk);
                for xk in before.shapes.difference(&owned.shapes) {
                    ops.push(sync::op_remove(&path_of(&["shape"]), xk));
                }
                for pk in before.paras.difference(&owned.paras) {
                    ops.push(sync::op_remove(&path_of(&["para"]), pk));
                }
                owned_now.shapes.extend(owned.shapes);
                owned_now.paras.extend(owned.paras);
                changed.push((sk, slide.clone()));
            }
            Ok(())
        })?;
        // Slides gone from the deck.
        let have = list_ids(view, &vec![], "slides").unwrap_or_default();
        for sk in &have {
            if !wanted_slides.contains(sk) {
                let before = owned_by(view, sk);
                for xk in &before.shapes {
                    ops.push(sync::op_remove(&path_of(&["shape"]), xk));
                }
                for pk in &before.paras {
                    ops.push(sync::op_remove(&path_of(&["para"]), pk));
                }
                ops.push(sync::op_remove(&path_of(&["slide"]), sk));
            }
        }
        put_list(&mut ops, view, &vec![], "slides", &wanted_slides);
        for (sk, s) in changed {
            synced.insert(sk, s);
        }
        synced.retain(|k, _| wanted_slides.contains(k));
        Ok(Pass { ops, owned: owned_now })
    }

    /// ★ A LOCAL EDIT: the batches that take what the peers hold to `deck`, or none when it is the same. Typing in one paragraph
    /// joins the previous undo step when it follows within 1.5 s.
    pub fn local(&mut self, deck: &Presentation, now_ms: u64) -> Result<Vec<Batch>, ShimError> {
        self.sync_ids_in(deck);
        let pass = Self::diff(&self.ids, &self.view, &mut self.synced, deck, false)?;
        let _ = pass.owned;
        if pass.ops.is_empty() {
            return Ok(Vec::new());
        }
        let typing = pass.ops.iter().all(|o| matches!(o.get("op").and_then(Value::as_str), Some("text.insert" | "text.delete")));
        let formatting = pass.ops.iter().all(|o| matches!(o.get("op").and_then(Value::as_str), Some("set" | "text.mark")));
        let join = typing && self.last_typing_ms.is_some_and(|t| now_ms.saturating_sub(t) < 1_500);
        self.last_typing_ms = if typing { Some(now_ms) } else { None };
        let label = if typing {
            "Typing"
        } else if formatting {
            "Formatting"
        } else {
            "Edit"
        };
        let mut batches = Vec::new();
        self.emit(pass.ops, label, join, false, now_ms, &mut batches)?;
        Ok(batches)
    }

    // ── reading the shared tree ───────────────────────────────────────────────────────────────────────────────────────

    fn read_para(&self, pk: &str) -> Result<Option<Paragraph>, ShimError> {
        let pp = path_of(&["para", pk]);
        // A listed paragraph with no map yet (a peer's batch in flight) is not shown yet.
        if node(&self.view, &pp).is_none() {
            return Ok(None);
        }
        let d = ParaD {
            text: sync::view_text(&self.view, &pp, "text").unwrap_or_default(),
            pv: read_value(&self.view, &pp, "pv").unwrap_or_else(|| json!({})),
        };
        para_from(&d).map(Some)
    }

    fn read_body(&self, path: &P, tbk: &str, tpk: &str) -> Result<Option<TextBody>, ShimError> {
        let Some(props) = read_value(&self.view, path, tbk) else { return Ok(None) };
        let mut body: TextBody = model("text body", props)?;
        for pk in list_ids(&self.view, path, tpk).unwrap_or_default() {
            if let Some(p) = self.read_para(&pk)? {
                body.paragraphs.push(p);
            }
        }
        Ok(Some(body))
    }

    fn read_shapes(&self, parent: &P, list: &str, depth: usize) -> Result<Vec<deckcraft_model::Shape>, ShimError> {
        if depth > MAX_DEPTH {
            return Err(ShimError::Model("groups are nested too deeply".into()));
        }
        let mut out = Vec::new();
        for xk in list_ids(&self.view, parent, list).unwrap_or_default() {
            let xp = path_of(&["shape", &xk]);
            // A listed shape with no map yet (a peer's batch in flight) is not shown yet.
            if node(&self.view, &xp).is_none() {
                continue;
            }
            let mut fields = read_fields(&self.view, &xp);
            fields.insert("id".to_owned(), Value::String(xk.clone()));
            if let Some(kind) = fields.get_mut("kind").and_then(Value::as_object_mut)
                && kind.get("type").and_then(Value::as_str) == Some("group")
            {
                kind.insert("children".to_owned(), json!([]));
            }
            let mut shape: deckcraft_model::Shape = model("a shape", Value::Object(fields))?;
            shape.text = self.read_body(&xp, "tb", "tp")?;
            if let ShapeKind::Group { children, .. } = &mut shape.kind {
                *children = self.read_shapes(&xp, "ch", depth + 1)?;
            }
            out.push(shape);
        }
        Ok(out)
    }

    fn read_slide(&self, sk: &str) -> Result<Option<Slide>, ShimError> {
        let sp = path_of(&["slide", sk]);
        if node(&self.view, &sp).is_none() {
            return Ok(None);
        }
        let mut fields = read_fields(&self.view, &sp);
        let id = self.ids.0.borrow_mut().slide_id_of(sk);
        fields.insert("id".to_owned(), json!(id.0));
        let mut slide: Slide = model("a slide", Value::Object(fields))?;
        slide.shapes = self.read_shapes(&sp, "shapes", 0)?;
        if let Some(n) = self.read_body(&sp, "nb", "np")? {
            slide.notes = n;
        }
        Ok(Some(slide))
    }

    /// Replace the slides of `deck` that `dirty` names (`None`: all) with what the view holds. Slides that read back the same keep
    /// their `Arc`. Answers whether the deck changed.
    fn show(&mut self, deck: &mut Presentation, dirty: Option<&BTreeSet<String>>) -> Result<bool, ShimError> {
        let ids = self.ids.clone();
        {
            let mut i = ids.0.borrow_mut();
            i.next_id = i.next_id.max(deck.next_id);
            i.media.refresh(&deck.media);
            i.new_media.clear();
        }
        let keys = list_ids(&self.view, &vec![], "slides").unwrap_or_default();
        let by_id: HashMap<SlideId, Arc<Slide>> = deck.slides.iter().map(|s| (s.id, s.clone())).collect();
        let mut next: Vec<Arc<Slide>> = Vec::with_capacity(keys.len());
        let this = &*self;
        ids.scope(|| -> Result<(), ShimError> {
            for sk in &keys {
                let known = ids.0.borrow().known_slide(sk);
                let held = known.and_then(|id| by_id.get(&id)).cloned();
                if let (Some(h), Some(d)) = (&held, dirty)
                    && !d.contains(sk)
                {
                    next.push(h.clone());
                    continue;
                }
                let Some(fresh) = this.read_slide(sk)? else { continue };
                match held {
                    Some(h) if *h == fresh => next.push(h),
                    _ => next.push(Arc::new(fresh)),
                }
            }
            Ok(())
        })?;
        // Pictures this window met an address for and has no bytes of: empty items stand in until they arrive.
        let made: Vec<(deckcraft_model::MediaId, String)> = std::mem::take(&mut ids.0.borrow_mut().new_media);
        for (id, _) in &made {
            deck.media.push(deckcraft_model::MediaItem { id: *id, name: "picture".into(), content_type: "application/octet-stream".into(), ..Default::default() });
        }
        ids.0.borrow_mut().media.refresh(&deck.media);
        deck.next_id = deck.next_id.max(ids.0.borrow().next_id);
        let changed = next.len() != deck.slides.len() || next.iter().zip(&deck.slides).any(|(a, b)| !Arc::ptr_eq(a, b)) || !made.is_empty();
        for s in &next {
            let sk = ids.0.borrow_mut().slide_key(s.id);
            self.synced.insert(sk, s.clone());
        }
        let live: BTreeSet<String> = keys.into_iter().collect();
        self.synced.retain(|k, _| live.contains(k));
        if changed {
            deck.slides = next;
        }
        self.shown.wanted = self.ids.0.borrow().media.wanted();
        Ok(changed)
    }

    /// The whole shared state arrived (a join, a resync after a refusal, a snapshot): show it. Batches waiting for an
    /// acknowledgement are dropped - the state is the host's truth.
    pub fn remote_state(&mut self, doc: &Value, deck: &mut Presentation) -> Result<bool, ShimError> {
        self.view = Mirror::new(doc.clone());
        self.confirmed = None;
        self.pending.clear();
        self.show(deck, None)
    }

    /// ★ A REMOTE CHANGE arrived: apply the host's `changes` (and `ack`: the last local batch it applied), put the batches still
    /// in flight back on top, and show the slides that changed. Answers whether the deck changed.
    pub fn remote_delta(&mut self, changes: &[Change], ack: u64, now_ms: u64, deck: &mut Presentation) -> Result<bool, ShimError> {
        let before = self.owner_index();
        let mut touched: BTreeSet<String> = BTreeSet::new();
        let mut note = |path: &[String], extra: Option<&str>| {
            if let Some(first) = path.first() {
                touched.insert(first.clone());
                if let Some(k) = path.get(1) {
                    touched.insert(k.clone());
                }
            }
            if let Some(k) = extra {
                touched.insert(k.to_owned());
            }
        };
        for c in changes {
            note(&c.path, None);
        }
        match self.confirmed.take() {
            Some(mut truth) => {
                // The host applied our batches up to `ack` before it read what it reports.
                while self.pending.first().is_some_and(|(seq, _, _)| *seq <= ack) {
                    let (_, _, ops) = self.pending.remove(0);
                    for op in &ops {
                        let _ = truth.apply(op);
                    }
                }
                for c in changes {
                    truth.apply_change(&c.path, c.value.as_ref())?;
                }
                let mut view = truth.clone();
                for (_, _, ops) in &self.pending {
                    for op in ops {
                        let path: Vec<String> =
                            op.get("path").and_then(Value::as_array).map(|p| p.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()).unwrap_or_default();
                        note(&path, op.get("key").and_then(Value::as_str));
                        let _ = view.apply(op);
                    }
                }
                self.view = view;
                self.confirmed = if self.pending.is_empty() { None } else { Some(truth) };
            }
            None => {
                for c in changes {
                    self.view.apply_change(&c.path, c.value.as_ref())?;
                }
            }
        }
        self.prune(now_ms);
        let after = self.owner_index();
        // Which slides a touched key belongs to; a key nobody owns yet (or any change to the order) rebuilds everything.
        let mut dirty: BTreeSet<String> = BTreeSet::new();
        let mut everything = false;
        for key in &touched {
            match key.as_str() {
                "slides" | "slide" | "shape" | "para" => {}
                k if k.starts_with('s') => {
                    dirty.insert(k.to_owned());
                }
                k => match before.get(k).into_iter().chain(after.get(k)).next() {
                    Some(_) => {
                        for sk in before.get(k).into_iter().chain(after.get(k)) {
                            dirty.insert(sk.clone());
                        }
                    }
                    None => everything = true,
                },
            }
        }
        let slides_touched = touched.contains("slides");
        let _ = slides_touched;
        let result = self.show(deck, if everything { None } else { Some(&dirty) })?;
        Ok(result)
    }

    /// Which slide owns each shape and paragraph key, as the view holds them now.
    fn owner_index(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for sk in list_ids(&self.view, &vec![], "slides").unwrap_or_default() {
            let o = owned_by(&self.view, &sk);
            for k in o.shapes.into_iter().chain(o.paras) {
                out.insert(k, sk.clone());
            }
        }
        out
    }

    /// Everything beside the slides that remote changes altered since the last call.
    pub fn take_shown(&mut self) -> Shown {
        let mut shown = std::mem::take(&mut self.shown);
        shown.wanted = self.ids.0.borrow().media.wanted();
        shown
    }

    // ── pictures ──────────────────────────────────────────────────────────────────────────────────────────────────────

    /// A picture's bytes were found (in the file's saved version): fill the placeholder that waits for `hash`. `false` when these
    /// bytes are not the picture `hash` names (a host that answers with other bytes is not believed), or nothing waits for it.
    pub fn media_arrived(&mut self, deck: &mut Presentation, hash: &str, item: &deckcraft_model::MediaItem) -> bool {
        if !MediaIndex::is_address_of(hash, &item.data) {
            return false;
        }
        let Some(id) = self.ids.0.borrow().media.placeholder_for(hash) else { return false };
        let Some(slot) = deck.media.iter_mut().find(|m| m.id == id) else { return false };
        slot.name.clone_from(&item.name);
        slot.content_type.clone_from(&item.content_type);
        slot.data = item.data.clone();
        self.ids.0.borrow_mut().media.refresh(&deck.media);
        // Slides that use it are shown again (their pictures now have bytes): drop what was synced so the next read replaces them.
        self.synced.clear();
        true
    }

    // ── cursors ───────────────────────────────────────────────────────────────────────────────────────────────────────

    /// The body of a `craft:crdt:cursor` message: the slide being edited and the shapes selected on it, by shared key.
    #[must_use]
    pub fn cursor_for(&self, slide: SlideId, shapes: &[ShapeId]) -> Option<Value> {
        let mut ids = self.ids.0.borrow_mut();
        let s = ids.slide_key(slide);
        let x: Vec<String> = shapes.iter().take(50).map(|id| ids.shape_key(*id)).collect();
        Some(json!({"s": s, "x": x}))
    }

    /// Another person's cursor body, resolved against this window's deck.
    #[must_use]
    pub fn peer_cursor(&self, cursor: &Value) -> Option<PeerCursor> {
        let ids = self.ids.0.borrow();
        let slide = ids.known_slide(cursor.get("s")?.as_str()?)?;
        let shapes = cursor.get("x").and_then(Value::as_array).map(|a| a.iter().filter_map(|k| k.as_str().and_then(|k| ids.known_shape(k))).collect()).unwrap_or_default();
        Some(PeerCursor { slide, shapes })
    }

    /// Everything this window shows that is shared, in a form that does not depend on this window's ids or on how runs are split
    /// (shapes by shared key, paragraphs as text with per-character marks): two windows that converged have equal digests.
    pub fn digest(&self, deck: &Presentation) -> Result<Value, ShimError> {
        let ids = self.ids.clone();
        ids.scope(|| -> Result<Value, ShimError> {
            fn body(b: &TextBody) -> Result<Value, ShimError> {
                let paras = b
                    .paragraphs
                    .iter()
                    .map(|p| {
                        let d = para_d(p)?;
                        Ok(json!({"text": d.text.chars.iter().collect::<String>(), "marks": d.text.marks, "pv": d.pv}))
                    })
                    .collect::<Result<Vec<_>, ShimError>>()?;
                Ok(json!({"props": canon(&body_props(b)?), "paras": paras}))
            }
            fn shapes(list: &[deckcraft_model::Shape], ids: &Ids) -> Result<Value, ShimError> {
                let mut out = Vec::new();
                for s in list {
                    let key = ids.0.borrow_mut().shape_key(s.id);
                    let children = match &s.kind {
                        ShapeKind::Group { children, .. } => shapes(children, ids)?,
                        _ => Value::Null,
                    };
                    let text = match &s.text {
                        Some(t) => body(t)?,
                        None => Value::Null,
                    };
                    out.push(json!({"key": key, "f": canon(&Value::Object(shape_fields(s)?)), "text": text, "children": children}));
                }
                Ok(Value::Array(out))
            }
            let mut slides = Vec::new();
            for s in &deck.slides {
                let key = ids.0.borrow_mut().slide_key(s.id);
                let comments: Vec<Value> = s.comments.iter().map(|c| json!({"text": c.text, "resolved": c.resolved, "replies": c.replies.iter().map(|r| r.text.clone()).collect::<Vec<_>>()})).collect();
                slides.push(json!({"key": key, "f": canon(&Value::Object(slide_fields(s)?)), "shapes": shapes(&s.shapes, &ids)?, "notes": body(&s.notes)?, "comments": comments}));
            }
            Ok(Value::Array(slides))
        })
    }

    /// The slide key of a slide of this window.
    #[must_use]
    pub fn slide_key(&self, id: SlideId) -> String {
        self.ids.0.borrow_mut().slide_key(id)
    }

    /// The shape key of a shape of this window.
    #[must_use]
    pub fn shape_key(&self, id: ShapeId) -> String {
        self.ids.0.borrow_mut().shape_key(id)
    }

    /// The plain value of a shared property (tests, tools).
    #[must_use]
    pub fn value_at(&self, path: &[&str]) -> Option<Value> {
        let p: P = path.iter().map(|s| (*s).to_owned()).collect();
        node(&self.view, &p).map(|v| plain(v).clone())
    }

    /// The deck's slide keys in order, as the view holds them.
    #[must_use]
    pub fn slide_keys(&self) -> Vec<String> {
        list_ids(&self.view, &vec![], "slides").unwrap_or_default()
    }

    /// The shared per-slide notes and shape count (for tests): `BTreeMap<slide key, shape keys>`.
    #[must_use]
    pub fn shapes_by_slide(&self) -> BTreeMap<String, Vec<String>> {
        self.slide_keys().into_iter().map(|sk| (sk.clone(), list_ids(&self.view, &path_of(&["slide", &sk]), "shapes").unwrap_or_default())).collect()
    }
}

#[cfg(test)]
mod tests;
