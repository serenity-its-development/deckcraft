//! Pictures (and every other embedded file) as content-addressed references: the host's op contract never puts bytes in the room.
//!
//! In the deck a picture names its bytes by a [`MediaId`] (a number in the deck's own table), which means nothing between windows:
//! two people inserting two pictures both get "media 7". What the shared tree holds is the CONTENT ADDRESS, `cas-<sha256 hex>`,
//! written wherever the deck says `MediaId` (see `deckcraft_model::idcodec`), so the same bytes are the same reference in every
//! window and different bytes never collide. The bytes themselves are not in the room: a window that meets an address it has no
//! bytes for makes an EMPTY placeholder item for it, lists the hash in [`MediaIndex::wanted`], and the host answers with the
//! file's latest saved version, which carries every picture; the window picks the bytes whose address it wants out of that file.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use deckcraft_model::{MediaId, MediaItem};
use sha2::{Digest, Sha256};

/// The prefix of a shared (content-addressed) media key.
pub const SHARED_PREFIX: &str = "cas-";

/// SHA-256 of `bytes`, lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// The shared form of a picture with this hash.
#[must_use]
pub fn shared_key(hash: &str) -> String {
    format!("{SHARED_PREFIX}{hash}")
}

/// The hash a shared key names (`None` if `key` is not a shared media key).
#[must_use]
pub fn hash_of_shared(key: &str) -> Option<&str> {
    let hash = key.strip_prefix(SHARED_PREFIX)?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hash)
}

/// Which pictures this window holds, by id and by content address.
#[derive(Debug, Default, Clone)]
pub struct MediaIndex {
    /// id -> (the bytes it was hashed from, their hash). The bytes are held so a changed entry is noticed by pointer.
    by_id: HashMap<MediaId, (Arc<Vec<u8>>, String)>,
    /// content address -> an id holding those bytes.
    by_hash: HashMap<String, MediaId>,
    /// Items made for an address whose bytes have not arrived: address -> id, and back.
    placeholder: HashMap<String, MediaId>,
    placeholder_hash: HashMap<MediaId, String>,
    /// Content addresses the shared tree refers to that this window has no bytes for.
    wanted: BTreeSet<String>,
}

impl MediaIndex {
    /// Bring the index up to date with the deck's media. Cheap when nothing changed (the bytes are compared by pointer).
    pub fn refresh(&mut self, media: &[MediaItem]) {
        let live: std::collections::HashSet<MediaId> = media.iter().map(|m| m.id).collect();
        self.by_id.retain(|id, _| live.contains(id));
        self.placeholder_hash.retain(|id, _| live.contains(id));
        for item in media {
            if item.data.is_empty() {
                // A placeholder (or a linked item): its address, if it is one this window waits on.
                self.by_id.remove(&item.id);
                continue;
            }
            let fresh = match self.by_id.get(&item.id) {
                Some((held, _)) => !Arc::ptr_eq(held, &item.data),
                None => true,
            };
            if fresh {
                self.by_id.insert(item.id, (item.data.clone(), sha256_hex(&item.data)));
            }
        }
        self.by_hash.clear();
        for (id, (_, hash)) in &self.by_id {
            // Several items may hold the same bytes: the smallest id is the one every rebuild picks.
            match self.by_hash.get(hash) {
                Some(have) if have <= id => {}
                _ => {
                    self.by_hash.insert(hash.clone(), *id);
                }
            }
        }
        let have = &self.by_hash;
        self.placeholder.retain(|h, id| !have.contains_key(h) && self.placeholder_hash.contains_key(id));
        self.wanted.retain(|h| !have.contains_key(h));
    }

    /// The content address of the picture an id names (its bytes' hash, or the address a placeholder waits on).
    #[must_use]
    pub fn hash_of(&self, id: MediaId) -> Option<&str> {
        self.by_id.get(&id).map(|(_, h)| h.as_str()).or_else(|| self.placeholder_hash.get(&id).map(String::as_str))
    }

    /// An id holding the bytes with this content address (or the placeholder waiting for them).
    #[must_use]
    pub fn id_of(&self, hash: &str) -> Option<MediaId> {
        self.by_hash.get(hash).or_else(|| self.placeholder.get(hash)).copied()
    }

    /// Record the placeholder made for `hash` (the deck now holds an empty item `id`), and that its bytes are wanted.
    pub fn placeholder_made(&mut self, hash: &str, id: MediaId) {
        self.placeholder.insert(hash.to_owned(), id);
        self.placeholder_hash.insert(id, hash.to_owned());
        self.wanted.insert(hash.to_owned());
    }

    /// The placeholder waiting for `hash`.
    #[must_use]
    pub fn placeholder_for(&self, hash: &str) -> Option<MediaId> {
        self.placeholder.get(hash).copied()
    }

    /// The content addresses to ask the host for.
    #[must_use]
    pub fn wanted(&self) -> Vec<String> {
        self.wanted.iter().cloned().collect()
    }

    /// Whether `bytes` is the picture `hash` names. A host that answers with the wrong bytes is not believed.
    #[must_use]
    pub fn is_address_of(hash: &str, bytes: &[u8]) -> bool {
        sha256_hex(bytes) == hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u32, bytes: &[u8]) -> MediaItem {
        MediaItem { id: MediaId(id), name: format!("m{id}.png"), content_type: "image/png".into(), data: Arc::new(bytes.to_vec()), link: None }
    }

    #[test]
    fn shared_keys_carry_the_address() {
        let h = sha256_hex(b"abc");
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hash_of_shared(&shared_key(&h)), Some(h.as_str()));
        assert_eq!(hash_of_shared("cas-xyz"), None);
        assert_eq!(hash_of_shared("image1.png"), None);
    }

    #[test]
    fn the_index_follows_the_media_and_forgets_what_arrived() {
        let mut idx = MediaIndex::default();
        let mut media = vec![item(9, b"same"), item(3, b"same")];
        idx.refresh(&media);
        let h = sha256_hex(b"same");
        assert_eq!(idx.hash_of(MediaId(9)), Some(h.as_str()));
        assert_eq!(idx.id_of(&h), Some(MediaId(3)), "the smallest id stands for equal bytes");
        let other = sha256_hex(b"other");
        idx.placeholder_made(&other, MediaId(20));
        media.push(MediaItem { id: MediaId(20), ..Default::default() });
        idx.refresh(&media);
        assert_eq!(idx.wanted(), vec![other.clone()]);
        assert_eq!(idx.id_of(&other), Some(MediaId(20)));
        assert_eq!(idx.hash_of(MediaId(20)), Some(other.as_str()));
        media[2] = item(20, b"other");
        idx.refresh(&media);
        assert!(idx.wanted().is_empty());
        assert!(MediaIndex::is_address_of(&other, b"other") && !MediaIndex::is_address_of(&other, b"nope"));
    }
}
