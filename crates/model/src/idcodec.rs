//! OneOS live editing: the SHARED form of the ids a presentation holds.
//!
//! Between windows a shape id or a media id means nothing: two people each add a shape and both get the next free number, two
//! people each insert a picture and both call theirs media 7. What the shared document holds instead is a KEY per shape (minted
//! once, the same in every window) and a CONTENT ADDRESS per picture (`cas-<sha256>`). A window serialises a slide to its shared
//! form with a codec installed ([`with_codec`]): every `ShapeId` and `MediaId` then writes as its key string, and reads a key
//! string back as this window's own id. With no codec installed (every ordinary use) the ids are the plain numbers they always
//! were, so the file formats are untouched.

use std::cell::RefCell;
use std::rc::Rc;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{MediaId, ShapeId};

/// What a live-editing window knows of the shared keys of its ids.
pub trait IdCodec {
    /// The shared key of a shape of this window (`None`: no key, written as the plain number).
    fn shape_out(&self, id: ShapeId) -> Option<String>;
    /// This window's shape for a shared key (made when the window has none yet).
    fn shape_in(&self, key: &str) -> Option<ShapeId>;
    /// The shared content address of a picture of this window.
    fn media_out(&self, id: MediaId) -> Option<String>;
    /// This window's media for a content address (a placeholder until the bytes arrive).
    fn media_in(&self, key: &str) -> Option<MediaId>;
}

thread_local! {
    static CODEC: RefCell<Option<Rc<dyn IdCodec>>> = const { RefCell::new(None) };
}

/// Run `f` with `codec` installed (restored after, also when `f` unwinds).
pub fn with_codec<R>(codec: Rc<dyn IdCodec>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Rc<dyn IdCodec>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CODEC.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CODEC.with(|c| c.borrow_mut().replace(codec)));
    f()
}

fn current() -> Option<Rc<dyn IdCodec>> {
    CODEC.with(|c| c.borrow().clone())
}

struct IdVisitor {
    what: &'static str,
}

impl Visitor<'_> for IdVisitor {
    type Value = Result<u32, String>;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a {} id (a number, or a shared key)", self.what)
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        u32::try_from(v).map(Ok).map_err(|_| E::custom("id out of range"))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        u32::try_from(v).map(Ok).map_err(|_| E::custom("id out of range"))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        if v >= 0.0 && v <= f64::from(u32::MAX) && v.fract() == 0.0 { Ok(Ok(v as u32)) } else { Err(E::custom("id out of range")) }
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        // A number used as a map key arrives as text.
        Ok(v.parse::<u32>().map_err(|_| v.to_owned()))
    }
}

macro_rules! coded_id {
    ($name:ident, $what:literal, $out:ident, $inn:ident) => {
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                match current().and_then(|c| c.$out(*self)) {
                    Some(key) => s.serialize_str(&key),
                    None => s.serialize_u32(self.0),
                }
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                match d.deserialize_any(IdVisitor { what: $what })? {
                    Ok(n) => Ok($name(n)),
                    Err(key) => {
                        current().and_then(|c| c.$inn(&key)).ok_or_else(|| de::Error::custom(format!("no {} for the shared key {key}", $what)))
                    }
                }
            }
        }
    };
}
coded_id!(ShapeId, "shape", shape_out, shape_in);
coded_id!(MediaId, "media", media_out, media_in);
