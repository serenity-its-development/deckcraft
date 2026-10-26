//! The keys of this window's ids (see `deckcraft_model::idcodec`): a slide or shape of this window has a SHARED KEY, minted the
//! first time it is shared, that every window uses for it; a key this window has not met gets a fresh id of its own from the
//! deck's counter. Pictures are shared by content address ([`crate::media`]).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use deckcraft_model::idcodec::{IdCodec, with_codec};
use deckcraft_model::{MediaId, ShapeId, SlideId};

use crate::media::{MediaIndex, hash_of_shared, shared_key};

#[derive(Debug, Default)]
pub struct IdsInner {
    rng: u64,
    minted: HashSet<String>,
    slide_by_id: HashMap<SlideId, String>,
    slide_by_key: HashMap<String, SlideId>,
    shape_by_id: HashMap<ShapeId, String>,
    shape_by_key: HashMap<String, ShapeId>,
    pub media: MediaIndex,
    /// The deck's id counter, advanced by the ids this window makes for keys it meets.
    pub next_id: u32,
    /// Placeholder pictures made while reading: (id, content address).
    pub new_media: Vec<(MediaId, String)>,
}

impl IdsInner {
    /// A fresh key: `<prefix>` and 12 hex digits, random per window.
    pub fn mint_key(&mut self, prefix: char) -> String {
        loop {
            // xorshift64*: keys only need to differ between the windows of one file; the seed is random per window.
            let mut x = self.rng;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.rng = x;
            let key = format!("{prefix}{:012x}", x.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 16);
            if self.minted.insert(key.clone()) {
                return key;
            }
        }
    }

    /// A fresh comment number: 31 bits, random per window (the top half is for the host's own panel's comments).
    pub fn mint_wc(&mut self) -> u32 {
        let k = self.mint_key('c');
        let n = u32::from_str_radix(&k[1..9], 16).unwrap_or(1) & 0x7fff_ffff;
        n.max(1)
    }

    fn alloc(&mut self) -> u32 {
        self.next_id = self.next_id.max(1).saturating_add(1);
        self.next_id
    }

    pub fn slide_key(&mut self, id: SlideId) -> String {
        if let Some(k) = self.slide_by_id.get(&id) {
            return k.clone();
        }
        let k = self.mint_key('s');
        self.slide_by_id.insert(id, k.clone());
        self.slide_by_key.insert(k.clone(), id);
        k
    }

    pub fn shape_key(&mut self, id: ShapeId) -> String {
        if let Some(k) = self.shape_by_id.get(&id) {
            return k.clone();
        }
        let k = self.mint_key('x');
        self.shape_by_id.insert(id, k.clone());
        self.shape_by_key.insert(k.clone(), id);
        k
    }

    /// This window's slide for a shared key (a fresh id when it has none).
    pub fn slide_id_of(&mut self, key: &str) -> SlideId {
        if let Some(id) = self.slide_by_key.get(key) {
            return *id;
        }
        let id = SlideId(self.alloc());
        self.slide_by_id.insert(id, key.to_owned());
        self.slide_by_key.insert(key.to_owned(), id);
        id
    }

    pub fn shape_id_of(&mut self, key: &str) -> ShapeId {
        if let Some(id) = self.shape_by_key.get(key) {
            return *id;
        }
        let id = ShapeId(self.alloc());
        self.shape_by_id.insert(id, key.to_owned());
        self.shape_by_key.insert(key.to_owned(), id);
        id
    }

    /// The slide a key names, if this window has met it.
    #[must_use]
    pub fn known_slide(&self, key: &str) -> Option<SlideId> {
        self.slide_by_key.get(key).copied()
    }

    #[must_use]
    pub fn known_shape(&self, key: &str) -> Option<ShapeId> {
        self.shape_by_key.get(key).copied()
    }
}

/// The shared handle: the shim holds one, and installs a clone as the codec while it reads or writes the model's JSON.
#[derive(Clone, Debug)]
pub struct Ids(pub Rc<RefCell<IdsInner>>);

impl Ids {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Ids(Rc::new(RefCell::new(IdsInner { rng: seed | 1, ..Default::default() })))
    }

    /// Run `f` with this window's ids as the codec.
    pub fn scope<R>(&self, f: impl FnOnce() -> R) -> R {
        with_codec(Rc::new(self.clone()), f)
    }
}

impl IdCodec for Ids {
    fn shape_out(&self, id: ShapeId) -> Option<String> {
        Some(self.0.borrow_mut().shape_key(id))
    }

    fn shape_in(&self, key: &str) -> Option<ShapeId> {
        Some(self.0.borrow_mut().shape_id_of(key))
    }

    fn media_out(&self, id: MediaId) -> Option<String> {
        self.0.borrow().media.hash_of(id).map(shared_key)
    }

    fn media_in(&self, key: &str) -> Option<MediaId> {
        let hash = hash_of_shared(key)?;
        let mut inner = self.0.borrow_mut();
        if let Some(id) = inner.media.id_of(hash) {
            return Some(id);
        }
        let id = MediaId(inner.alloc());
        inner.media.placeholder_made(hash, id);
        inner.new_media.push((id, hash.to_owned()));
        Some(id)
    }
}
