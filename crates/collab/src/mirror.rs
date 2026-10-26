//! The mirror: the shared document tree the host sends the frame (`craft:crdt:state` / `craft:crdt:delta`), and the ONE
//! operation vocabulary that changes it. This is the frame's copy of the room's tree under the `craft` root:
//!
//! * a map is a JSON object; an object-valued property is `{"$value": v}`;
//! * a text is `{"$text": [{"text": "...", "marks": {...}}, ...]}` (marks kept, in spans);
//! * a list of scalars is `{"$list": [...]}`.
//!
//! [`Mirror::apply`] applies one op of the host's contract (`set`, `remove`, `new`, `ensure`, `text.insert`,
//! `text.delete`, `text.mark`, `list.insert`, `list.delete`) with the engine's semantics: text indexes are characters
//! (Unicode scalars), a mark whose value is null clears the key, a container exists only after `new`/`ensure`.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// The marks of one character.
pub type Marks = BTreeMap<String, Value>;
/// A text as one entry per character.
pub type Chars = Vec<(char, Marks)>;

/// A path segment list into the tree, `["blocks", "<id>", "text"]`.
pub type Path = Vec<String>;

/// An op the mirror could not apply (the room would refuse it too).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorError(pub String);

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, MirrorError> {
    Err(MirrorError(msg.into()))
}

/// Spans (`[{text, marks}]`) as one entry per character.
#[must_use]
pub fn spans_to_chars(spans: &Value) -> Chars {
    let mut out = Chars::new();
    let Some(list) = spans.as_array() else { return out };
    for span in list {
        let text = span.get("text").and_then(Value::as_str).unwrap_or_default();
        let marks: Marks = span
            .get("marks")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        for c in text.chars() {
            out.push((c, marks.clone()));
        }
    }
    out
}

/// One entry per character as spans (adjacent characters with equal marks share a span).
#[must_use]
pub fn chars_to_spans(chars: &Chars) -> Value {
    let mut spans: Vec<(String, &Marks)> = Vec::new();
    for (c, m) in chars {
        match spans.last_mut() {
            Some((text, marks)) if *marks == m => text.push(*c),
            _ => spans.push((c.to_string(), m)),
        }
    }
    Value::Array(
        spans
            .into_iter()
            .map(|(text, marks)| {
                let marks: Map<String, Value> = marks.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                json!({"text": text, "marks": marks})
            })
            .collect(),
    )
}

/// The shared tree (the `doc` object of the host's state).
#[derive(Debug, Clone, PartialEq)]
pub struct Mirror {
    pub root: Value,
}

impl Default for Mirror {
    fn default() -> Self {
        Mirror { root: Value::Object(Map::new()) }
    }
}

fn node_at<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut at = root;
    for seg in path {
        at = at.as_object_mut()?.get_mut(seg)?;
    }
    Some(at)
}

fn text_chars(node: &Value) -> Option<Chars> {
    node.get("$text").map(spans_to_chars)
}

fn put_chars(node: &mut Value, chars: &Chars) {
    if let Some(o) = node.as_object_mut() {
        o.insert("$text".to_owned(), chars_to_spans(chars));
    }
}

/// A scalar or object property value as the tree holds it.
fn property(value: &Value) -> Value {
    if value.is_object() { json!({"$value": value}) } else { value.clone() }
}

fn container(kind: &str, text: Option<&str>) -> Result<Value, MirrorError> {
    Ok(match kind {
        "map" => Value::Object(Map::new()),
        "list" => json!({"$list": []}),
        "text" => {
            let chars: Chars = text.unwrap_or_default().chars().map(|c| (c, Marks::new())).collect();
            json!({"$text": chars_to_spans(&chars)})
        }
        other => return err(format!("no container kind {other}")),
    })
}

fn as_index(v: Option<&Value>, what: &str) -> Result<usize, MirrorError> {
    v.and_then(Value::as_u64).map(|n| n as usize).ok_or_else(|| MirrorError(format!("{what} is not an index")))
}

impl Mirror {
    #[must_use]
    pub fn new(root: Value) -> Self {
        Mirror { root: if root.is_object() { root } else { Value::Object(Map::new()) } }
    }

    /// The node at `path`, if there is one.
    #[must_use]
    pub fn get(&self, path: &[&str]) -> Option<&Value> {
        let mut at = &self.root;
        for seg in path {
            at = at.as_object()?.get(*seg)?;
        }
        Some(at)
    }

    /// Set (or remove) the value at `path` of a host change: `['doc', ...]` is stripped by the caller.
    pub fn apply_change(&mut self, path: &[String], value: Option<&Value>) -> Result<(), MirrorError> {
        let Some((leaf, parents)) = path.split_last() else {
            if let Some(v) = value {
                *self = Mirror::new(v.clone());
            }
            return Ok(());
        };
        let mut at = &mut self.root;
        for seg in parents {
            let obj = at.as_object_mut().ok_or_else(|| MirrorError(format!("{seg}: not a map")))?;
            at = obj.entry(seg.clone()).or_insert_with(|| Value::Object(Map::new()));
            if !at.is_object() {
                *at = Value::Object(Map::new());
            }
        }
        let obj = at.as_object_mut().ok_or_else(|| MirrorError("not a map".into()))?;
        match value {
            Some(v) => {
                obj.insert(leaf.clone(), v.clone());
            }
            None => {
                obj.remove(leaf);
            }
        }
        Ok(())
    }

    /// Apply one op of the host's contract.
    pub fn apply(&mut self, op: &Value) -> Result<(), MirrorError> {
        let kind = op.get("op").and_then(Value::as_str).unwrap_or_default();
        let path: Path = op
            .get("path")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let key = op.get("key").and_then(Value::as_str);
        let node = node_at(&mut self.root, &path).ok_or_else(|| MirrorError(format!("craft-path-missing: nothing is at {}", path.join("/"))))?;
        match kind {
            "set" | "remove" | "new" | "ensure" => {
                let key = key.ok_or_else(|| MirrorError(format!("{kind}: no key")))?;
                let map = node.as_object_mut().ok_or_else(|| MirrorError(format!("craft-path-kind: {} is not a map", path.join("/"))))?;
                match kind {
                    "set" => {
                        let value = op.get("value").ok_or_else(|| MirrorError("set: no value".into()))?;
                        map.insert(key.to_owned(), property(value));
                    }
                    "remove" => {
                        map.remove(key);
                    }
                    "new" => {
                        let made = container(op.get("kind").and_then(Value::as_str).unwrap_or_default(), op.get("text").and_then(Value::as_str))?;
                        map.insert(key.to_owned(), made);
                    }
                    _ => {
                        if !map.contains_key(key) {
                            map.insert(key.to_owned(), container(op.get("kind").and_then(Value::as_str).unwrap_or_default(), None)?);
                        }
                    }
                }
                Ok(())
            }
            "text.insert" | "text.delete" | "text.mark" => {
                let mut chars = text_chars(node).ok_or_else(|| MirrorError(format!("craft-path-kind: {} is not a text", path.join("/"))))?;
                match kind {
                    "text.insert" => {
                        let at = as_index(op.get("at"), "at")?;
                        if at > chars.len() {
                            return err(format!("IndexOutOfRange: {at} is past the end ({})", chars.len()));
                        }
                        let text = op.get("text").and_then(Value::as_str).unwrap_or_default();
                        let ins: Chars = text.chars().map(|c| (c, Marks::new())).collect();
                        chars.splice(at..at, ins);
                    }
                    "text.delete" => {
                        let at = as_index(op.get("at"), "at")?;
                        let len = as_index(op.get("len"), "len")?;
                        if at + len > chars.len() {
                            return err(format!("IndexOutOfRange: {at}+{len} is past the end ({})", chars.len()));
                        }
                        chars.drain(at..at + len);
                    }
                    _ => {
                        let start = as_index(op.get("start"), "start")?;
                        let end = as_index(op.get("end"), "end")?;
                        if end > chars.len() || start >= end {
                            return err(format!("IndexOutOfRange: {start}..{end} in a text of {}", chars.len()));
                        }
                        let mark = op.get("key").and_then(Value::as_str).ok_or_else(|| MirrorError("text.mark: no key".into()))?;
                        let value = op.get("value").cloned().unwrap_or(Value::Null);
                        for (_, marks) in &mut chars[start..end] {
                            if value.is_null() {
                                marks.remove(mark);
                            } else {
                                marks.insert(mark.to_owned(), value.clone());
                            }
                        }
                    }
                }
                put_chars(node, &chars);
                Ok(())
            }
            "list.insert" | "list.delete" => {
                let list = node
                    .get_mut("$list")
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| MirrorError(format!("craft-path-kind: {} is not a list", path.join("/"))))?;
                let at = as_index(op.get("at"), "at")?;
                if kind == "list.insert" {
                    if at > list.len() {
                        return err(format!("IndexOutOfRange: {at} is past the end ({})", list.len()));
                    }
                    list.insert(at, op.get("value").cloned().unwrap_or(Value::Null));
                } else {
                    let len = as_index(op.get("len"), "len")?;
                    if at + len > list.len() {
                        return err(format!("IndexOutOfRange: {at}+{len} is past the end ({})", list.len()));
                    }
                    list.drain(at..at + len);
                }
                Ok(())
            }
            other => err(format!("no op {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_ops_follow_the_engine() {
        let mut m = Mirror::default();
        m.apply(&json!({"op": "new", "path": [], "key": "t", "kind": "text", "text": "héllo"})).unwrap();
        m.apply(&json!({"op": "text.insert", "path": ["t"], "at": 5, "text": " wörld"})).unwrap();
        m.apply(&json!({"op": "text.mark", "path": ["t"], "start": 0, "end": 5, "key": "c:bold", "value": true})).unwrap();
        m.apply(&json!({"op": "text.delete", "path": ["t"], "at": 1, "len": 1})).unwrap();
        let chars = spans_to_chars(&m.root["t"]["$text"]);
        assert_eq!(chars.iter().map(|(c, _)| *c).collect::<String>(), "hllo wörld");
        assert!(chars[0].1.contains_key("c:bold") && !chars[6].1.contains_key("c:bold"));
        m.apply(&json!({"op": "text.mark", "path": ["t"], "start": 0, "end": 2, "key": "c:bold", "value": null})).unwrap();
        assert!(!spans_to_chars(&m.root["t"]["$text"])[0].1.contains_key("c:bold"));
    }

    #[test]
    fn a_missing_path_is_refused_by_name_and_a_container_needs_new() {
        let mut m = Mirror::default();
        let e = m.apply(&json!({"op": "set", "path": ["blocks", "x"], "key": "a", "value": 1})).unwrap_err();
        assert!(e.0.starts_with("craft-path-missing"));
        m.apply(&json!({"op": "ensure", "path": [], "key": "blocks", "kind": "map"})).unwrap();
        m.apply(&json!({"op": "set", "path": ["blocks"], "key": "p", "value": {"a": 1}})).unwrap();
        assert_eq!(m.root["blocks"]["p"], json!({"$value": {"a": 1}}));
    }

    #[test]
    fn lists_insert_and_delete() {
        let mut m = Mirror::default();
        m.apply(&json!({"op": "new", "path": [], "key": "body", "kind": "list"})).unwrap();
        for (i, id) in ["a", "b", "c"].iter().enumerate() {
            m.apply(&json!({"op": "list.insert", "path": ["body"], "at": i, "value": id})).unwrap();
        }
        m.apply(&json!({"op": "list.delete", "path": ["body"], "at": 1, "len": 1})).unwrap();
        assert_eq!(m.root["body"], json!({"$list": ["a", "c"]}));
        assert!(m.apply(&json!({"op": "list.insert", "path": ["body"], "at": 9, "value": "z"})).is_err());
    }
}
