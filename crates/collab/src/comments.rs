//! Comments, through the comments tier the host already holds (design 12.5, 12.9 Q2).
//!
//! The host owns comments: each is a map in the file's sibling comments room, `{by, name, at, text, resolved, parent?,
//! anchor?}`, stamped with the signed-in party (a frame cannot claim an author), and the host moves a text anchor with a
//! `cmt:<id>` mark on its characters. This shim does not put comments in the document tree. It only translates:
//!
//! * the slides' comments (a slide's list of comments, each with its replies, flattened to one entry each) become
//!   `craft:crdt:comment` messages ([`CommentMsg`]): add, reply, edit, resolve, reopen, delete;
//! * the host's `comments` map (in `craft:crdt:state`, and in a delta whose path is `["comments"]`) becomes comments on the
//!   slides ([`CommentsUpdate`]).
//!
//! A comment's number `wc` (a `u32` this window draws at random for a comment it meets first) travels in the comment's anchor
//! beside the slide and shape it is on, so every window names a comment by the same number. A comment made through the host's
//! own panel carries no `wc`; its number is derived from its id ([`derived_wc`]), the same in every window.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

/// One `craft:crdt:comment` message (without the session).
#[derive(Debug, Clone, PartialEq)]
pub enum CommentMsg {
    Add { wc: u32, text: String, anchor: Value },
    Reply { parent: String, wc: u32, text: String },
    Edit { id: String, text: String },
    Resolve { id: String },
    Reopen { id: String },
    Delete { id: String },
}

impl CommentMsg {
    /// The frame message body: `{action, id?, text?, anchor?}`.
    #[must_use]
    pub fn body(&self) -> Value {
        match self {
            CommentMsg::Add { text, anchor, .. } => json!({"action": "add", "text": text, "anchor": anchor}),
            CommentMsg::Reply { parent, wc, text } => json!({"action": "reply", "id": parent, "text": text, "anchor": {"wc": wc}}),
            CommentMsg::Edit { id, text } => json!({"action": "edit", "id": id, "text": text}),
            CommentMsg::Resolve { id } => json!({"action": "resolve", "id": id}),
            CommentMsg::Reopen { id } => json!({"action": "reopen", "id": id}),
            CommentMsg::Delete { id } => json!({"action": "delete", "id": id}),
        }
    }
}

/// A comment the host holds that the editor should show.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteComment {
    pub wc: u32,
    pub author: String,
    pub initials: String,
    /// ISO 8601, UTC.
    pub date: String,
    pub parent: Option<u32>,
    pub resolved: bool,
    pub text: String,
    /// The anchor the comment was made with (`{wc, slide, shape?}`; a comment from the host's own panel has none).
    pub anchor: Value,
}

/// What the host's comments changed in the editor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommentsUpdate {
    pub upsert: Vec<RemoteComment>,
    pub remove: Vec<u32>,
}

impl CommentsUpdate {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upsert.is_empty() && self.remove.is_empty()
    }
}

/// What both sides agree a comment is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    text: String,
    resolved: bool,
    parent: Option<u32>,
}

/// The comment number of a comment the host made without one: stable across windows, in the top half of the id space so it
/// cannot meet the numbers an editor counts up from.
#[must_use]
pub fn derived_wc(host_id: &str) -> u32 {
    // FNV-1a.
    let mut h: u32 = 0x811c_9dc5;
    for b in host_id.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    0x8000_0000 | (h & 0x7fff_ffff)
}

/// Initials of a display name ("Ann Lee" is "AL").
#[must_use]
pub fn initials_of(name: &str) -> String {
    let mut out: String = name.split_whitespace().filter_map(|w| w.chars().next()).flat_map(char::to_uppercase).take(3).collect();
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// A millisecond Unix time as ISO 8601 UTC (`2026-10-26T01:02:03Z`).
#[must_use]
pub fn iso_of(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A comment of this window, as the sync sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalComment {
    pub text: String,
    pub resolved: bool,
    pub parent: Option<u32>,
    /// `{slide, shape?}` (a reply has none: it follows its parent).
    pub anchor: Value,
}

/// The comment sync state of one window.
#[derive(Debug, Default)]
pub struct CommentSync {
    /// Comments both sides hold, as last agreed.
    known: BTreeMap<u32, Known>,
    /// The host's id of each comment number.
    host_of: HashMap<u32, String>,
    /// Comments deleted here before the host told us their id: deleted once it does.
    deleted_unknown: HashSet<u32>,
    /// Messages waiting on a host id.
    outbox: Vec<CommentMsg>,
}

impl CommentSync {
    /// The messages that take the host's comments to the editor's. `anchor_of(wc)` finds where a new comment is anchored in the
    /// shared body.
    pub fn local(&mut self, comments: &BTreeMap<u32, LocalComment>) -> Vec<CommentMsg> {
        let mut out = std::mem::take(&mut self.outbox);
        // Deleted here.
        let gone: Vec<u32> = self.known.keys().filter(|wc| !comments.contains_key(wc)).copied().collect();
        let gone_set: std::collections::HashSet<u32> = gone.iter().copied().collect();
        for wc in gone {
            let parent = self.known.get(&wc).and_then(|k| k.parent);
            self.known.remove(&wc);
            // A reply that went because its thread was deleted is not deleted with it: the host lets only an author delete a
            // comment, and a reply by someone else is theirs. It comes back from the host as a comment of its own.
            if parent.is_some_and(|p| gone_set.contains(&p)) {
                continue;
            }
            match self.host_of.get(&wc) {
                Some(id) => out.push(CommentMsg::Delete { id: id.clone() }),
                None => {
                    self.deleted_unknown.insert(wc);
                }
            }
        }
        for (wc, c) in comments {
            let want = Known { text: c.text.clone(), resolved: c.resolved, parent: c.parent };
            match self.known.get(wc) {
                None => {
                    if want.text.trim().is_empty() {
                        // The host takes no empty comment: it is sent when it has words.
                        continue;
                    }
                    let msg = match c.parent.and_then(|p| self.host_of.get(&p)) {
                        Some(parent) => CommentMsg::Reply { parent: parent.clone(), wc: *wc, text: want.text.clone() },
                        None if c.parent.is_some() => continue, // its parent has no host id yet
                        None => {
                            let mut anchor = c.anchor.clone();
                            if let Some(o) = anchor.as_object_mut() {
                                o.insert("wc".to_owned(), json!(wc));
                            }
                            CommentMsg::Add { wc: *wc, text: want.text.clone(), anchor }
                        }
                    };
                    out.push(msg);
                    self.known.insert(*wc, want);
                }
                Some(have) if *have != want => {
                    let Some(id) = self.host_of.get(wc).cloned() else { continue };
                    if have.text != want.text && !want.text.trim().is_empty() {
                        out.push(CommentMsg::Edit { id: id.clone(), text: want.text.clone() });
                    }
                    if have.resolved != want.resolved {
                        out.push(if want.resolved { CommentMsg::Resolve { id } } else { CommentMsg::Reopen { id } });
                    }
                    self.known.insert(*wc, want);
                }
                Some(_) => {}
            }
        }
        out
    }

    /// The host's `comments` map arrived: what the editor should show, and any delete that was waiting on an id.
    pub fn remote(&mut self, host: &Value) -> (CommentsUpdate, Vec<CommentMsg>) {
        let mut update = CommentsUpdate::default();
        let mut deletes = Vec::new();
        let Some(map) = host.as_object() else {
            return (update, deletes);
        };
        // The numbers first, so a reply can name its parent.
        let mut by_host: HashMap<&str, u32> = HashMap::new();
        for (id, c) in map {
            let wc = c
                .get("anchor")
                .and_then(|a| a.get("wc"))
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or_else(|| derived_wc(id));
            by_host.insert(id.as_str(), wc);
        }
        for (id, wc) in &by_host {
            self.host_of.insert(*wc, (*id).to_owned());
        }
        for (id, c) in map {
            let wc = by_host[id.as_str()];
            if self.deleted_unknown.remove(&wc) {
                deletes.push(CommentMsg::Delete { id: id.clone() });
                continue;
            }
            let text = c.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            let resolved = c.get("resolved").and_then(Value::as_bool) == Some(true);
            let parent = c.get("parent").and_then(Value::as_str).and_then(|p| by_host.get(p).copied());
            let want = Known { text: text.clone(), resolved, parent };
            if self.known.get(&wc) == Some(&want) {
                continue;
            }
            self.known.insert(wc, want);
            let name = c.get("name").and_then(Value::as_str).unwrap_or("Someone").to_owned();
            let at = c.get("at").and_then(Value::as_u64).unwrap_or(0);
            update.upsert.push(RemoteComment {
                wc,
                initials: initials_of(&name),
                author: name,
                date: iso_of(at),
                parent,
                resolved,
                text,
                anchor: c.get("anchor").cloned().unwrap_or(Value::Null),
            });
        }
        // Gone from the host.
        let live: HashSet<u32> = by_host.values().copied().collect();
        let removed: Vec<u32> = self.known.keys().filter(|wc| !live.contains(wc) && self.host_of.contains_key(wc)).copied().collect();
        for wc in removed {
            self.known.remove(&wc);
            self.host_of.remove(&wc);
            update.remove.push(wc);
        }
        (update, deletes)
    }

    /// Whether the host has named this comment (its id is known).
    #[must_use]
    pub fn host_knows(&self, wc: u32) -> bool {
        self.host_of.contains_key(&wc)
    }

    /// Put messages that could not be sent yet back.
    pub fn hold(&mut self, msgs: Vec<CommentMsg>) {
        self.outbox.extend(msgs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(text: &str) -> LocalComment {
        LocalComment { text: text.into(), resolved: false, parent: None, anchor: json!({"slide": "s1"}) }
    }

    #[test]
    fn iso_and_initials() {
        assert_eq!(iso_of(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_of(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(initials_of("ann lee"), "AL");
        assert_eq!(initials_of(""), "?");
        assert_eq!(derived_wc("c1"), derived_wc("c1"));
        assert!(derived_wc("c1") >= 0x8000_0000);
    }

    #[test]
    fn a_new_comment_is_added_once_and_its_changes_wait_for_the_host_id() {
        let mut sync = CommentSync::default();
        let mut comments = BTreeMap::from([(5u32, local("first"))]);
        let sent = sync.local(&comments);
        assert_eq!(sent.len(), 1);
        assert!(matches!(&sent[0], CommentMsg::Add { wc: 5, text, anchor } if text == "first" && anchor["slide"] == "s1" && anchor["wc"] == 5));
        assert!(sync.local(&comments).is_empty(), "sent once");
        comments.get_mut(&5).unwrap().text = "first, edited".into();
        assert!(sync.local(&comments).is_empty(), "an edit before the host named the comment is held");
        let host = json!({"h1": {"by": "p", "name": "Ann Lee", "at": 1000, "text": "first", "resolved": false, "anchor": {"wc": 5, "slide": "s1"}}});
        let (update, deletes) = sync.remote(&host);
        assert!(update.is_empty() && deletes.is_empty(), "our own comment coming back changes nothing: {update:?}");
        assert_eq!(sync.local(&comments), vec![CommentMsg::Edit { id: "h1".into(), text: "first, edited".into() }]);
        comments.get_mut(&5).unwrap().resolved = true;
        assert_eq!(sync.local(&comments), vec![CommentMsg::Resolve { id: "h1".into() }]);
        comments.remove(&5);
        assert_eq!(sync.local(&comments), vec![CommentMsg::Delete { id: "h1".into() }]);
    }

    #[test]
    fn the_hosts_comments_reach_the_deck_and_leave_it() {
        let mut sync = CommentSync::default();
        let host = json!({
            "h1": {"name": "Ben Ray", "at": 1_700_000_000_000u64, "text": "look here", "resolved": false, "anchor": {"wc": 9, "slide": "s2"}},
            "h2": {"name": "Cy", "at": 1_700_000_001_000u64, "text": "agreed", "resolved": true, "parent": "h1"},
        });
        let (update, _) = sync.remote(&host);
        assert_eq!(update.upsert.len(), 2);
        let h1 = update.upsert.iter().find(|c| c.wc == 9).unwrap();
        assert_eq!((h1.author.as_str(), h1.initials.as_str(), h1.parent, h1.anchor["slide"].as_str()), ("Ben Ray", "BR", None, Some("s2")));
        let h2 = update.upsert.iter().find(|c| c.wc == derived_wc("h2")).unwrap();
        assert_eq!((h2.parent, h2.resolved), (Some(9), true));
        assert!(sync.remote(&host).0.is_empty());
        let host = json!({"h1": {"name": "Ben Ray", "at": 1_700_000_000_000u64, "text": "look there", "resolved": false, "anchor": {"wc": 9, "slide": "s2"}}});
        let (update, _) = sync.remote(&host);
        assert_eq!(update.upsert.len(), 1);
        assert_eq!(update.remove, vec![derived_wc("h2")]);
    }
}
