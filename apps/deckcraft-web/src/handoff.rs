//! The editor's half of the OneOS progressive open (Mona 2026-10-27, "can we stream the app in?").
//!
//! The host keeps its read-only viewer on screen as the FIRST VIEW while this editor streams in. It sends `craft:open`; this module
//! says, by events and not by a timer, when the host may let the first view go:
//!
//! 1. `craft:opened {session}` the moment the opened document is loaded and has been drawn (laid out and painted [`DRAWN_FRAMES`] frames):
//!    the editor takes input from here.
//! 2. the host answers `craft:view {session, scrollTop, scrollHeight, clientHeight, selection ...}` (the loader keeps it on
//!    `globalThis.__craftFirstView`): the editor opens at the same place, the app's own idea of one (a text offset, a cell, a slide and
//!    an object, a page and an offset, a frame; the `apply` closure of the app);
//! 3. `craft:viewed {session}` once the editor has PAINTED that place ([`APPLIED_FRAMES`] frames later): the host drops the first view
//!    and replays what was typed meanwhile.
//!
//! A host that hears neither shows the editor after its own bounded, logged fallback. Pure: the host is a [`Link`], the app's document
//! identity a value that changes when a new document is opened (so a test drives it frame by frame).

/// Frames the opened document is drawn before the editor says `craft:opened` (the first lays it out, the second paints it).
pub const DRAWN_FRAMES: u8 = 2;
/// Frames the editor asks for while it waits for the opened document to replace the blank one (then it waits without asking).
pub const LOADING_REPAINT_FRAMES: u32 = 120;
/// Frames the first view's place is drawn before the editor says `craft:viewed`.
pub const APPLIED_FRAMES: u16 = 2;
/// The most frames `craft:viewed` waits for an app that is still settling the place.
pub const BUSY_MAX_FRAMES: u16 = 180;

/// Where the host's first view was left (`globalThis.__craftFirstView`, set by the frame's loader from `craft:view`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FirstView {
    pub session: String,
    /// Counts the host's `craft:view` messages: a second one (the person scrolled on) is a new place.
    pub seq: u64,
    pub scroll_top: f64,
    pub scroll_left: f64,
    pub scroll_height: f64,
    pub scroll_width: f64,
    pub client_height: f64,
    pub client_width: f64,
    /// The text selected in the first view ('' none).
    pub selection: String,
}

fn fraction(at: f64, whole: f64, shown: f64) -> f64 {
    let span = whole - shown;
    if at.is_finite() && span.is_finite() && span > 0.5 { (at / span).clamp(0.0, 1.0) } else { 0.0 }
}

impl FirstView {
    /// How far down the first view was, 0 (top) to 1 (end of the scrollable height).
    pub fn y_fraction(&self) -> f64 {
        fraction(self.scroll_top, self.scroll_height, self.client_height)
    }
    /// How far across.
    pub fn x_fraction(&self) -> f64 {
        fraction(self.scroll_left, self.scroll_width, self.client_width)
    }
}

/// The host, as the app's bridge speaks to it.
pub trait Link {
    /// How many `craft:open` have arrived (a new open is a new count).
    fn open_seq(&self) -> u64;
    /// The first view's place the host sent for the current open, once.
    fn take_view(&self) -> Option<FirstView>;
    /// Post `craft:opened`.
    fn post_opened(&self);
    /// Post `craft:viewed`.
    fn post_viewed(&self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No open waiting.
    Idle,
    /// `craft:open` arrived; the document it names is not the one shown yet.
    Loading,
    /// The document is loaded: drawing it.
    Drawing(u8),
    /// `craft:opened` sent; waiting for the host's `craft:view`.
    Opened,
    /// The place applied: drawing it.
    Applying(u16),
}

/// One editor's progress through the hand-over, per `craft:open`.
pub struct Handoff<D> {
    seen: u64,
    baseline: Option<D>,
    phase: Phase,
    /// Frames spent waiting for the document to replace the blank one.
    waited: u32,
}

impl<D: PartialEq + Clone> Default for Handoff<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: PartialEq + Clone> Handoff<D> {
    pub fn new() -> Self {
        Self { seen: 0, baseline: None, phase: Phase::Idle, waited: 0 }
    }

    /// Once per frame BEFORE the app's own pass (which opens the file from its inbox): a new `craft:open` is noted against the document
    /// the app shows right now, so the open has happened once the app shows another.
    pub fn before(&mut self, link: &impl Link, document: D) {
        let seq = link.open_seq();
        if seq != self.seen {
            self.seen = seq;
            self.baseline = Some(document);
            self.phase = Phase::Loading;
            self.waited = 0;
        }
    }

    /// Once per frame AFTER the app's own pass. `apply` puts the editor at the host's first view (called once per `craft:view`); the
    /// result is whether another frame is wanted (the app asks its context to repaint).
    pub fn after(&mut self, link: &impl Link, document: D, apply: impl FnOnce(&FirstView)) -> bool {
        self.after_with(link, document, apply, false)
    }

    /// As [`Self::after`], with `busy`: the app is still settling the place (an app that finds the first view's text as its pages are
    /// read), so `craft:viewed` waits for it - for at most [`BUSY_MAX_FRAMES`] frames.
    pub fn after_with(&mut self, link: &impl Link, document: D, apply: impl FnOnce(&FirstView), busy: bool) -> bool {
        match self.phase {
            Phase::Idle => false,
            Phase::Loading => {
                if self.baseline.as_ref() != Some(&document) {
                    self.phase = Phase::Drawing(0);
                    return true;
                }
                // Not loaded yet: ask for frames for a while (the open lands within a few), then wait quietly - whatever lands the
                // document wakes the app - so a file that never opens is not a repaint loop.
                self.waited = self.waited.saturating_add(1);
                self.waited <= LOADING_REPAINT_FRAMES
            }
            Phase::Drawing(n) => {
                if n + 1 >= DRAWN_FRAMES {
                    link.post_opened();
                    self.phase = Phase::Opened;
                } else {
                    self.phase = Phase::Drawing(n + 1);
                }
                true
            }
            Phase::Opened => {
                if let Some(view) = link.take_view() {
                    apply(&view);
                    self.phase = Phase::Applying(0);
                }
                // Waiting for the host costs no frames: its message wakes the app.
                self.phase == Phase::Applying(0)
            }
            Phase::Applying(n) => {
                if n + 1 >= APPLIED_FRAMES && (!busy || n >= BUSY_MAX_FRAMES) {
                    link.post_viewed();
                    self.phase = Phase::Idle;
                    false
                } else {
                    self.phase = Phase::Applying(n.saturating_add(1));
                    true
                }
            }
        }
    }
}

/// The browser's side of [`Link`]: the loader's `globalThis.__craftFirstView` and the count of opens (one page is one thread).
#[cfg(target_arch = "wasm32")]
pub mod wasm {
    use super::FirstView;
    use std::cell::Cell;
    use wasm_bindgen::JsValue;

    thread_local! {
        static OPENS: Cell<u64> = const { Cell::new(0) };
        static TAKEN: Cell<u64> = const { Cell::new(0) };
    }

    /// A `craft:open` arrived.
    pub fn note_open() {
        OPENS.with(|c| c.set(c.get().wrapping_add(1)));
    }
    /// How many `craft:open` have arrived.
    pub fn opens() -> u64 {
        OPENS.with(Cell::get)
    }

    fn number(o: &JsValue, key: &str) -> f64 {
        js_sys::Reflect::get(o, &JsValue::from_str(key)).ok().and_then(|v| v.as_f64()).filter(|n| n.is_finite()).unwrap_or(0.0)
    }

    /// The place the loader kept from the host's latest `craft:view` for this `session`, once per message.
    pub fn take_view(session: &str) -> Option<FirstView> {
        let global = js_sys::global();
        let o = js_sys::Reflect::get(&global, &JsValue::from_str("__craftFirstView")).ok().filter(|v| v.is_object())?;
        let seq = number(&o, "seq") as u64;
        let said = js_sys::Reflect::get(&o, &JsValue::from_str("session")).ok().and_then(|v| v.as_string()).unwrap_or_default();
        if session.is_empty() || said != session || seq <= TAKEN.with(Cell::get) {
            return None;
        }
        TAKEN.with(|c| c.set(seq));
        Some(FirstView {
            session: said,
            seq,
            scroll_top: number(&o, "scrollTop"),
            scroll_left: number(&o, "scrollLeft"),
            scroll_height: number(&o, "scrollHeight"),
            scroll_width: number(&o, "scrollWidth"),
            client_height: number(&o, "clientHeight"),
            client_width: number(&o, "clientWidth"),
            selection: js_sys::Reflect::get(&o, &JsValue::from_str("selection"))
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default()
                .chars()
                .take(4000)
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct Fake {
        seq: Cell<u64>,
        view: RefCell<Option<FirstView>>,
        posted: RefCell<Vec<&'static str>>,
    }
    impl Link for Fake {
        fn open_seq(&self) -> u64 {
            self.seq.get()
        }
        fn take_view(&self) -> Option<FirstView> {
            self.view.borrow_mut().take()
        }
        fn post_opened(&self) {
            self.posted.borrow_mut().push("opened");
        }
        fn post_viewed(&self) {
            self.posted.borrow_mut().push("viewed");
        }
    }

    fn frame(h: &mut Handoff<u64>, link: &Fake, doc: u64, applied: &mut Vec<FirstView>) -> bool {
        h.before(link, doc);
        h.after(link, doc, |v| applied.push(v.clone()))
    }

    #[test]
    fn opened_fires_once_after_the_document_is_drawn_and_not_before() {
        let link = Fake::default();
        let mut h = Handoff::<u64>::new();
        let mut applied = Vec::new();
        for _ in 0..5 {
            frame(&mut h, &link, 1, &mut applied);
        }
        assert!(link.posted.borrow().is_empty(), "nothing opened yet: the host has sent nothing");
        link.seq.set(1);
        // The open arrived but the app still shows document 1: not loaded, however many frames pass.
        for _ in 0..5 {
            frame(&mut h, &link, 1, &mut applied);
        }
        assert!(link.posted.borrow().is_empty());
        // The document changes: drawn for DRAWN_FRAMES frames, then opened - once.
        frame(&mut h, &link, 2, &mut applied);
        assert!(link.posted.borrow().is_empty(), "loaded but not yet drawn");
        for _ in 0..10 {
            frame(&mut h, &link, 2, &mut applied);
        }
        assert_eq!(*link.posted.borrow(), vec!["opened"]);
    }

    #[test]
    fn the_first_view_is_applied_exactly_once_and_viewed_follows_its_painted_frames() {
        let link = Fake::default();
        let mut h = Handoff::<u64>::new();
        let mut applied = Vec::new();
        link.seq.set(1);
        frame(&mut h, &link, 1, &mut applied); // the open arrives while the app still shows its blank document
        for _ in 0..4 {
            frame(&mut h, &link, 2, &mut applied);
        }
        assert_eq!(*link.posted.borrow(), vec!["opened"]);
        assert!(applied.is_empty(), "the host has not sent the place yet");
        *link.view.borrow_mut() = Some(FirstView {
            session: "s".into(),
            seq: 1,
            scroll_top: 500.0,
            scroll_height: 1500.0,
            client_height: 500.0,
            selection: "fox".into(),
            ..FirstView::default()
        });
        frame(&mut h, &link, 2, &mut applied);
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].selection, "fox");
        assert!((applied[0].y_fraction() - 0.5).abs() < 1e-9);
        assert_eq!(*link.posted.borrow(), vec!["opened"], "viewed waits for the place to be painted");
        for _ in 0..6 {
            frame(&mut h, &link, 2, &mut applied);
        }
        assert_eq!(applied.len(), 1);
        assert_eq!(*link.posted.borrow(), vec!["opened", "viewed"]);
    }

    #[test]
    fn a_second_open_starts_the_hand_over_again() {
        let link = Fake::default();
        let mut h = Handoff::<u64>::new();
        let mut applied = Vec::new();
        link.seq.set(1);
        frame(&mut h, &link, 1, &mut applied); // the open arrives while the app still shows its blank document
        for _ in 0..4 {
            frame(&mut h, &link, 2, &mut applied);
        }
        *link.view.borrow_mut() = Some(FirstView::default());
        for _ in 0..6 {
            frame(&mut h, &link, 2, &mut applied);
        }
        link.seq.set(2);
        frame(&mut h, &link, 2, &mut applied);
        for _ in 0..6 {
            frame(&mut h, &link, 3, &mut applied);
        }
        assert_eq!(*link.posted.borrow(), vec!["opened", "viewed", "opened"]);
    }

    #[test]
    fn viewed_waits_for_an_app_still_settling_the_place_but_only_so_long() {
        let link = Fake::default();
        let mut h = Handoff::<u64>::new();
        link.seq.set(1);
        h.before(&link, 1);
        for _ in 0..4 {
            h.before(&link, 2);
            h.after(&link, 2, |_| {});
        }
        *link.view.borrow_mut() = Some(FirstView::default());
        for _ in 0..40 {
            h.before(&link, 2);
            h.after_with(&link, 2, |_| {}, true);
        }
        assert_eq!(*link.posted.borrow(), vec!["opened"], "busy: viewed is held");
        for _ in 0..200 {
            h.before(&link, 2);
            h.after_with(&link, 2, |_| {}, true);
        }
        assert_eq!(*link.posted.borrow(), vec!["opened", "viewed"], "a bounded wait: viewed goes once");
    }

    #[test]
    fn a_file_that_never_opens_is_not_a_repaint_loop_but_a_late_one_still_opens() {
        let link = Fake::default();
        let mut h = Handoff::<u64>::new();
        link.seq.set(1);
        let mut asked = 0;
        for _ in 0..(LOADING_REPAINT_FRAMES * 3) {
            h.before(&link, 1);
            if h.after(&link, 1, |_| {}) {
                asked += 1;
            }
        }
        assert_eq!(asked, LOADING_REPAINT_FRAMES, "frames are asked for a while, then not");
        assert!(link.posted.borrow().is_empty());
        // The document lands late: the hand-over carries on.
        for _ in 0..4 {
            h.before(&link, 2);
            h.after(&link, 2, |_| {});
        }
        assert_eq!(*link.posted.borrow(), vec!["opened"]);
    }

    #[test]
    fn fractions_are_clamped_and_hostile_numbers_are_the_top() {
        let v = FirstView {
            scroll_top: 9000.0,
            scroll_height: 1000.0,
            client_height: 500.0,
            scroll_left: f64::NAN,
            scroll_width: 10.0,
            client_width: 10.0,
            ..FirstView::default()
        };
        assert_eq!(v.y_fraction(), 1.0);
        assert_eq!(v.x_fraction(), 0.0);
        assert_eq!(FirstView { scroll_top: 5.0, scroll_height: f64::INFINITY, client_height: 1.0, ..FirstView::default() }.y_fraction(), 0.0);
    }
}
