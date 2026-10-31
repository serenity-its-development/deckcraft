//! The primitives that turn "what the editor shows" into ops against "what the shared tree holds" (the view): values, lists
//! and CRDT text. Every function here compares against the VIEW, never against a copy of what was last sent, so a remote change
//! that arrived in between is part of the base; the caller applies the ops it gets to the view before diffing again.
//!
//! Numbers: a value goes out in canonical form (every number a float) and comes back in the engine's own spelling (20 for
//! 20.0), so values are COMPARED canonically ([`canon`]) and READ normalised ([`normalize`]: a whole float becomes an integer,
//! which both integer and float fields of the model accept).

use std::collections::BTreeSet;

use serde_json::{Map, Number, Value, json};

use crate::ShimError;
use crate::mirror::{Marks, Mirror, spans_to_chars};

/// The largest text one `new` / `text.insert` carries (the host's limit is 65 536 characters).
pub const CHUNK: usize = 60_000;
/// The largest JSON value one `set` carries (the host's limit is 64 KiB).
pub const MAX_VALUE: usize = 60_000;
/// The mark key of a run-properties field.
pub const FIELD: &str = "c:";
/// The mark key of an inline object (a line break, a field, an equation).
pub const OBJECT: &str = "obj";
/// The character an inline object occupies.
pub const OBJ_CHAR: char = '\u{FFFC}';

/// A path into the shared tree.
pub type P = Vec<String>;

#[must_use]
pub fn child(p: &P, key: &str) -> P {
    let mut q = p.clone();
    q.push(key.to_owned());
    q
}

#[must_use]
pub fn path_of(segs: &[&str]) -> P {
    segs.iter().map(|s| (*s).to_owned()).collect()
}

/// Every number a float, objects and arrays recursively: the form values are compared and sent in.
#[must_use]
pub fn canon(v: &Value) -> Value {
    match v {
        Value::Number(n) => n.as_f64().and_then(Number::from_f64).map_or_else(|| v.clone(), Value::Number),
        Value::Array(a) => Value::Array(a.iter().map(canon).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), canon(x))).collect()),
        other => other.clone(),
    }
}

/// A whole float as an integer, recursively: the form a value is read back into the model in.
pub fn normalize(v: &mut Value) {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && !n.is_i64()
                && !n.is_u64()
                && f.fract() == 0.0
                && f.abs() < 9.0e15
            {
                *v = Value::Number(Number::from(f as i64));
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalize),
        Value::Object(o) => o.values_mut().for_each(normalize),
        _ => {}
    }
}

/// The value a map property holds (an object property is `{"$value": v}`).
#[must_use]
pub fn plain(v: &Value) -> &Value {
    v.get("$value").unwrap_or(v)
}

fn bounded(what: &str, v: &Value) -> Result<(), ShimError> {
    let n = v.to_string().len();
    if n > MAX_VALUE {
        Err(ShimError::TooLarge(format!("{what} is {n} bytes of data - more than one shared value holds ({MAX_VALUE})")))
    } else {
        Ok(())
    }
}

/// The node of the view at a path.
#[must_use]
pub fn node<'a>(view: &'a Mirror, path: &[String]) -> Option<&'a Value> {
    let mut at = &view.root;
    for seg in path {
        at = at.as_object()?.get(seg)?;
    }
    Some(at)
}

/// The node at `path`/`key`.
#[must_use]
pub fn node_in<'a>(view: &'a Mirror, path: &[String], key: &str) -> Option<&'a Value> {
    node(view, path)?.as_object()?.get(key)
}

pub fn op_new(path: &P, key: &str, kind: &str) -> Value {
    json!({"op": "new", "path": path, "key": key, "kind": kind})
}

pub fn op_set(path: &P, key: &str, value: &Value) -> Value {
    json!({"op": "set", "path": path, "key": key, "value": value})
}

pub fn op_remove(path: &P, key: &str) -> Value {
    json!({"op": "remove", "path": path, "key": key})
}

/// Make a map at `path`/`key` unless it is there.
pub fn ensure_map(ops: &mut Vec<Value>, view: &Mirror, path: &P, key: &str) {
    if node_in(view, path, key).is_none() {
        ops.push(op_new(path, key, "map"));
    }
}

/// Make the property `key` of the map at `path` hold `want` (`None`: not exist).
pub fn put_value(ops: &mut Vec<Value>, view: &Mirror, path: &P, key: &str, want: Option<&Value>, what: &str) -> Result<(), ShimError> {
    let have = node_in(view, path, key);
    match (want, have) {
        (Some(w), Some(h)) => {
            let w = canon(w);
            if canon(plain(h)) != w {
                bounded(what, &w)?;
                ops.push(op_set(path, key, &w));
            }
        }
        (Some(w), None) => {
            let w = canon(w);
            bounded(what, &w)?;
            ops.push(op_set(path, key, &w));
        }
        (None, Some(_)) => ops.push(op_remove(path, key)),
        (None, None) => {}
    }
    Ok(())
}

/// The property `key` of the map at `path`, as a plain JSON value, read into the model's form.
#[must_use]
pub fn read_value(view: &Mirror, path: &[String], key: &str) -> Option<Value> {
    let mut v = plain(node_in(view, path, key)?).clone();
    normalize(&mut v);
    Some(v)
}

/// Fields: the properties of the map at `path` whose key starts with `f:`, as an object (read into the model's form).
#[must_use]
pub fn read_fields(view: &Mirror, path: &[String]) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(map) = node(view, path).and_then(Value::as_object) {
        for (k, v) in map {
            if let Some(name) = k.strip_prefix("f:") {
                let mut x = plain(v).clone();
                normalize(&mut x);
                out.insert(name.to_owned(), x);
            }
        }
    }
    out
}

/// Make the `f:` properties of the map at `path` exactly `fields`.
pub fn put_fields(ops: &mut Vec<Value>, view: &Mirror, path: &P, fields: &Map<String, Value>, what: &str) -> Result<(), ShimError> {
    for (name, want) in fields {
        put_value(ops, view, path, &format!("f:{name}"), Some(want), what)?;
    }
    if let Some(map) = node(view, path).and_then(Value::as_object) {
        for k in map.keys() {
            if let Some(name) = k.strip_prefix("f:")
                && !fields.contains_key(name)
            {
                ops.push(op_remove(path, k));
            }
        }
    }
    Ok(())
}

// ── lists ───────────────────────────────────────────────────────────────────────────────────────────────────────────

/// The ids a list at `path`/`key` holds (`None`: there is no list).
#[must_use]
pub fn list_ids(view: &Mirror, path: &[String], key: &str) -> Option<Vec<String>> {
    node_in(view, path, key)?.get("$list").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
}

/// Longest increasing subsequence of `seq`: the indices (into `seq`) of one.
fn lis(seq: &[usize]) -> Vec<usize> {
    let n = seq.len();
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; n];
    for i in 0..n {
        let at = tails.partition_point(|&t| seq[t] < seq[i]);
        if at > 0 {
            prev[i] = tails[at - 1];
        }
        if at == tails.len() {
            tails.push(i);
        } else {
            tails[at] = i;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied();
    while let Some(i) = k {
        out.push(i);
        k = (prev[i] != usize::MAX).then_some(prev[i]);
    }
    out.reverse();
    out
}

/// The ops that take the list `old` to `new` (both of unique ids): deletes of what is gone or moved, then inserts left to right.
#[must_use]
pub fn list_ops(path: &P, old: &[String], new: &[String]) -> Vec<Value> {
    let new_at: std::collections::HashMap<&str, usize> = new.iter().enumerate().map(|(i, k)| (k.as_str(), i)).collect();
    // What stays: the old ids that are still wanted, in the longest run that keeps the new order.
    let wanted: Vec<(usize, usize)> = old.iter().enumerate().filter_map(|(i, k)| new_at.get(k.as_str()).map(|n| (i, *n))).collect();
    let seq: Vec<usize> = wanted.iter().map(|(_, n)| *n).collect();
    let keep: BTreeSet<usize> = lis(&seq).into_iter().map(|j| wanted[j].0).collect();
    let mut ops = Vec::new();
    // Delete from the end so earlier indexes stay put; one op per run.
    let mut i = old.len();
    while i > 0 {
        if keep.contains(&(i - 1)) {
            i -= 1;
            continue;
        }
        let end = i;
        while i > 0 && !keep.contains(&(i - 1)) {
            i -= 1;
        }
        ops.push(json!({"op": "list.delete", "path": path, "at": i, "len": end - i}));
    }
    let mut cur: Vec<&str> = old.iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, k)| k.as_str()).collect();
    for (at, k) in new.iter().enumerate() {
        if cur.get(at) != Some(&k.as_str()) {
            ops.push(json!({"op": "list.insert", "path": path, "at": at, "value": k}));
            cur.insert(at, k.as_str());
        }
    }
    ops
}

/// Make the list at `path`/`key` hold `want`, making the list when there is none.
pub fn put_list(ops: &mut Vec<Value>, view: &Mirror, path: &P, key: &str, want: &[String]) {
    let have = match list_ids(view, path, key) {
        Some(h) => h,
        None => {
            // `ensure`, not `new`: two people giving one shape its first paragraphs (or a shape its first children) at once must
            // MERGE their lists, not have the second replace the first (the host's `map.ensureContainer`).
            ops.push(json!({"op": "ensure", "path": path, "key": key, "kind": "list"}));
            Vec::new()
        }
    };
    if have != want {
        ops.extend(list_ops(&child(path, key), &have, want));
    }
}

// ── text ────────────────────────────────────────────────────────────────────────────────────────────────────────────

/// A text: its characters and the marks of each.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Text {
    pub chars: Vec<char>,
    pub marks: Vec<Marks>,
}

impl Text {
    pub fn push(&mut self, c: char, marks: &Marks) {
        self.chars.push(c);
        self.marks.push(marks.clone());
    }
}

/// The text at `path`/`key` in the view, marks canonical.
#[must_use]
pub fn view_text(view: &Mirror, path: &[String], key: &str) -> Option<Text> {
    let spans = node_in(view, path, key)?.get("$text")?;
    let mut t = Text::default();
    for (c, marks) in spans_to_chars(spans) {
        t.chars.push(c);
        t.marks.push(marks.into_iter().map(|(k, v)| (k, canon(&v))).collect());
    }
    Some(t)
}

/// The mark ops that take per-character marks `from` to `to` (equal lengths): one op per run of characters whose value of a key
/// changes, a null value clearing it.
#[must_use]
pub fn mark_ops(path: &P, from: &[Marks], to: &[Marks]) -> Vec<Value> {
    debug_assert_eq!(from.len(), to.len());
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for m in from.iter().chain(to.iter()) {
        keys.extend(m.keys().map(String::as_str));
    }
    let mut ops = Vec::new();
    for key in keys {
        let mut i = 0;
        while i < to.len() {
            let (want, have) = (to[i].get(key), from[i].get(key));
            if want == have {
                i += 1;
                continue;
            }
            let start = i;
            while i < to.len() && to[i].get(key) == want && from[i].get(key) != want {
                i += 1;
            }
            ops.push(json!({"op": "text.mark", "path": path, "start": start, "end": i, "key": key, "value": want.cloned().unwrap_or(Value::Null), "expand": false}));
        }
    }
    ops
}

fn chunks(text: &[char]) -> impl Iterator<Item = String> + '_ {
    text.chunks(CHUNK).map(|c| c.iter().collect())
}

/// Make the text at `path`/`key` hold `want`: a new text with its marks, or the one span that changed (prefix and suffix kept)
/// and the marks that differ.
pub fn put_text(ops: &mut Vec<Value>, view: &Mirror, path: &P, key: &str, want: &Text) -> Result<(), ShimError> {
    let tp = child(path, key);
    let Some(old) = view_text(view, path, key) else {
        let mut it = chunks(&want.chars);
        match it.next() {
            Some(first) => ops.push(json!({"op": "new", "path": path, "key": key, "kind": "text", "text": first})),
            None => ops.push(op_new(path, key, "text")),
        }
        let mut at = CHUNK.min(want.chars.len());
        for more in it {
            let n = more.chars().count();
            ops.push(json!({"op": "text.insert", "path": tp, "at": at, "text": more}));
            at += n;
        }
        let blank = vec![Marks::new(); want.marks.len()];
        ops.extend(mark_ops(&tp, &blank, &want.marks));
        return Ok(());
    };
    if old == *want {
        return Ok(());
    }
    let (oc, nc) = (&old.chars, &want.chars);
    let mut p = 0;
    while p < oc.len() && p < nc.len() && oc[p] == nc[p] {
        p += 1;
    }
    let mut s = 0;
    while s < oc.len() - p && s < nc.len() - p && oc[oc.len() - 1 - s] == nc[nc.len() - 1 - s] {
        s += 1;
    }
    let (del, ins) = (oc.len() - p - s, &nc[p..nc.len() - s]);
    if del > 0 {
        ops.push(json!({"op": "text.delete", "path": tp, "at": p, "len": del}));
    }
    let mut at = p;
    for chunk in chunks(ins) {
        let n = chunk.chars().count();
        ops.push(json!({"op": "text.insert", "path": tp, "at": at, "text": chunk}));
        at += n;
    }
    // What the shared text holds now: the old marks outside the change, none inside it.
    let mut between: Vec<Marks> = Vec::with_capacity(want.marks.len());
    between.extend_from_slice(&old.marks[..p]);
    between.extend(std::iter::repeat_n(Marks::new(), ins.len()));
    between.extend_from_slice(&old.marks[old.marks.len() - s..]);
    ops.extend(mark_ops(&tp, &between, &want.marks));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Apply list ops to a plain Vec, as the engine would.
    fn run(old: &[String], ops: &[Value]) -> Vec<String> {
        let mut list = old.to_vec();
        for op in ops {
            let at = op["at"].as_u64().unwrap() as usize;
            match op["op"].as_str().unwrap() {
                "list.delete" => {
                    list.drain(at..at + op["len"].as_u64().unwrap() as usize);
                }
                _ => list.insert(at, op["value"].as_str().unwrap().to_owned()),
            }
        }
        list
    }

    #[test]
    fn list_ops_reach_the_target_for_inserts_deletes_and_moves() {
        let cases = [
            (ids(&["a", "b", "c"]), ids(&["a", "b", "c"])),
            (ids(&["a", "b", "c"]), ids(&["c", "a", "b"])),
            (ids(&["a", "b", "c", "d"]), ids(&["d", "x", "b", "y"])),
            (ids(&[]), ids(&["a", "b"])),
            (ids(&["a", "b"]), ids(&[])),
            (ids(&["a", "b", "c", "d", "e"]), ids(&["e", "d", "c", "b", "a"])),
        ];
        for (old, new) in cases {
            let ops = list_ops(&vec![], &old, &new);
            assert_eq!(run(&old, &ops), new, "{old:?} -> {new:?}");
        }
        assert!(list_ops(&vec![], &ids(&["a", "b"]), &ids(&["a", "b"])).is_empty());
        // A single move is one delete and one insert.
        assert_eq!(list_ops(&vec![], &ids(&["a", "b", "c"]), &ids(&["b", "c", "a"])).len(), 2);
    }

    #[test]
    fn numbers_compare_canonically_and_read_as_integers() {
        assert_eq!(canon(&json!({"a": 20, "b": [1.5, 2]})), canon(&json!({"a": 20.0, "b": [1.5, 2.0]})));
        let mut v = json!({"level": 1.0, "size": 20.5, "n": [3.0]});
        normalize(&mut v);
        assert_eq!(v["level"].as_u64(), Some(1));
        assert_eq!(v["size"].as_f64(), Some(20.5));
        assert_eq!(v["n"][0].as_u64(), Some(3));
    }
}
