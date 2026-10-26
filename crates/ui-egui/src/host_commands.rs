//! The assistant's command surface for the OneOS host bridge (`craft:command {method: "command.batch"}`).
//!
//! The host relays an allow-listed run: `{label, steps: [{name, args?}]}`. The whole run is judged BEFORE anything runs (an id
//! outside the list refuses the run, `command-not-allowed`), every step runs through the same registry the menus use (so a step
//! the deck's state forbids is refused exactly as it would be for the person), and the run is ONE undo step named by `label`: a
//! step that fails takes back every step before it, so a run is all or nothing. There is no file-path or dialog command in the
//! list: files move only through the host.
//!
//! This list is the one declared in the host's `platform-craft` registry as `slides` commands (pinned equal there).

use serde_json::{Value, json};

use crate::SlideApp;

/// Registry commands the assistant may run. Each acts at once on the deck (no dialog, no file picker, no pointer gesture), and
/// none is Undo or Redo: undoing a run is the person's.
pub const ASSISTANT_COMMANDS: &[&str] = &[
    // Slides.
    "slide.new",
    "slide.duplicate",
    "slide.delete",
    "slide.move",
    "slide.hide",
    "slide.layout",
    "slide.reset",
    "slide.rename",
    "slide.go",
    "section.add",
    "section.rename",
    "section.removeAll",
    "section.move",
    // Shapes: choosing, arranging, naming.
    "edit.select",
    "edit.selectAll",
    "edit.deselect",
    "edit.duplicate",
    "edit.delete",
    "arrange.bringToFront",
    "arrange.bringForward",
    "arrange.sendBackward",
    "arrange.sendToBack",
    "arrange.group",
    "arrange.ungroup",
    "arrange.rotateLeft",
    "arrange.rotateRight",
    "arrange.flipHorizontal",
    "arrange.flipVertical",
    "arrange.nudge",
    "shape.resize",
    "shape.setBounds",
    "shape.rename",
    "shape.altText",
    "shape.visible",
    "insert.textBox",
    // Text.
    "text.insert",
    "format.bold",
    "format.italic",
    "format.font",
    "format.size",
    "format.color",
    "format.clear",
    "format.alignLeft",
    "format.alignCenter",
    "format.alignRight",
    "format.justify",
    "format.indent",
    "format.outdent",
    "format.anchor",
    // Tables.
    "table.insertRowAbove",
    "table.insertRowBelow",
    "table.insertColumnLeft",
    "table.insertColumnRight",
    "table.deleteRow",
    "table.deleteColumn",
    "table.merge",
    "table.distributeRows",
    "table.distributeColumns",
    "table.rowHeight",
    // Look.
    "design.theme",
    "design.variant",
    "design.effects",
    "design.hideBackgroundGraphics",
    "transition.options",
    "transition.applyAll",
    // Comments.
    "comment.add",
    "comment.reply",
    "comment.delete",
    "comment.resolve",
];

/// Commands the window itself provides (they take arguments, which the registry's commands do not).
pub const ARGUMENT_COMMANDS: &[&str] = &[
    // {names?: [shape names], indices?: [0-based positions in the slide's stacking order], all?: bool}: select shapes of the
    // current slide, so the commands that act on the selection have something to act on.
    "select.shapes",
    // Read-only: the slide count, the current slide, its shapes (index, id, name, kind, text) and what is selected.
    "inspect",
];

/// Every command id a run may name, in the order the host's registry declares them.
pub fn allowed() -> Vec<&'static str> {
    ASSISTANT_COMMANDS.iter().chain(ARGUMENT_COMMANDS).copied().collect()
}

const MAX_STEPS: usize = 50;
const MAX_LABEL: usize = 120;

impl SlideApp {
    /// One `command.batch`: all of it, as ONE undo step, or none of it.
    pub fn run_command_batch(&mut self, params: &Value) -> Result<Value, String> {
        let label = params.get("label").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if label.chars().count() > MAX_LABEL {
            return Err(format!("label-too-long: a run's label is at most {MAX_LABEL} characters"));
        }
        let steps = params.get("steps").and_then(Value::as_array).ok_or("run-empty: a run states its steps")?;
        if steps.is_empty() {
            return Err("run-empty: a run states at least one command".into());
        }
        if steps.len() > MAX_STEPS {
            return Err(format!("run-too-long: a run is at most {MAX_STEPS} steps"));
        }
        // Judge the whole run first; nothing has run when one id is outside the list.
        let mut named = Vec::with_capacity(steps.len());
        for step in steps {
            let name = step.get("name").and_then(Value::as_str).unwrap_or("");
            if !ASSISTANT_COMMANDS.contains(&name) && !ARGUMENT_COMMANDS.contains(&name) {
                return Err(format!("command-not-allowed: Slides does not allow the command '{name}' -- nothing was run"));
            }
            named.push((name, step.get("args").cloned().unwrap_or_else(|| json!({}))));
        }
        if self.session.active().is_none() {
            return Err("no-document: no presentation is open in this window".into());
        }
        let depth = self.session.undo_depth();
        let mut results = Vec::with_capacity(named.len());
        for (i, (name, args)) in named.iter().enumerate() {
            match self.run_step(name, args) {
                Ok(result) => results.push(json!({ "name": name, "result": result })),
                Err(e) => {
                    self.session.take_back_to(depth);
                    return Err(format!("step-failed: step {} ({name}): {e} -- nothing was changed", i + 1));
                }
            }
        }
        let folded = self.session.merge_undo(depth, if label.is_empty() { "Assistant" } else { label });
        Ok(json!({ "steps": results, "undoStep": folded }))
    }

    fn run_step(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "select.shapes" => self.select_shapes(args),
            "inspect" => self.inspect_deck(),
            other => self.run(other, args.clone()),
        }
    }

    fn select_shapes(&mut self, args: &Value) -> Result<Value, String> {
        let d = self.session.active().ok_or("no presentation is open")?;
        let shapes = d.shapes();
        let ids: Vec<u32> = if args.get("all").and_then(Value::as_bool) == Some(true) {
            shapes.iter().map(|s| s.id.0).collect()
        } else {
            let mut ids = Vec::new();
            for n in args.get("names").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let found = shapes.iter().filter(|s| s.name == n).map(|s| s.id.0).collect::<Vec<_>>();
                if found.is_empty() {
                    return Err(format!("no shape named '{n}' on this slide"));
                }
                ids.extend(found);
            }
            for i in args.get("indices").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_u64) {
                let s = shapes.get(i as usize).ok_or_else(|| format!("no shape at index {i} on this slide"))?;
                ids.push(s.id.0);
            }
            ids
        };
        self.run("edit.select", json!({ "ids": ids }))
    }

    fn inspect_deck(&mut self) -> Result<Value, String> {
        let d = self.session.active().ok_or("no presentation is open")?;
        let shapes: Vec<Value> = d
            .shapes()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let text: String = s.text.as_ref().map(|t| t.text()).unwrap_or_default().chars().take(120).collect();
                json!({"index": i, "id": s.id.0, "name": s.name, "text": text})
            })
            .collect();
        Ok(json!({
            "slides": d.doc.slides.len(),
            "current": d.selection.slide,
            "slide": d.current_slide().map(|s| json!({"id": s.id.0, "name": s.name, "hidden": s.hidden, "comments": s.comments.len()})),
            "shapes": shapes,
            "selected": d.selection.shapes.iter().map(|i| i.0).collect::<Vec<_>>(),
            "undo": d.history.undo.last().map(|e| e.label.clone()),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use deckcraft_engine::Session;

    fn app() -> SlideApp {
        let mut app = SlideApp::new(Session::with_new(), Services::default());
        app.run("slide.new", json!({"layout": "titleAndContent", "title": "Agenda", "body": "One\nTwo"})).expect("slide");
        app
    }

    #[test]
    fn every_listed_command_exists_in_the_registry() {
        let app = app();
        let known: Vec<String> = app.session.commands().into_iter().map(|c| c.id.to_string()).collect();
        for id in ASSISTANT_COMMANDS {
            assert!(known.iter().any(|k| k == id), "{id} is not a registry command");
        }
        for bad in ["file.save", "file.open", "file.export", "file.saveAs", "edit.undo", "edit.redo", "show.fromStart", "app.print"] {
            assert!(!allowed().contains(&bad), "{bad} must not be allowed");
        }
        for id in allowed() {
            assert!(!id.contains("file") && !id.contains("path") && !id.contains("open") && !id.contains("save") && !id.contains("export") && !id.contains("import"), "{id}");
        }
    }

    #[test]
    fn a_run_is_judged_whole_first_and_is_one_undo_step() {
        let mut app = app();
        let before = app.session.undo_depth();
        let refused = app.run_command_batch(&json!({"label": "x", "steps": [{"name": "slide.duplicate"}, {"name": "file.save"}]}));
        assert!(refused.expect_err("outside the list").starts_with("command-not-allowed"));
        assert_eq!(app.session.undo_depth(), before, "nothing ran");
        let ok = app
            .run_command_batch(&json!({"label": "Duplicate and rename", "steps": [{"name": "slide.duplicate"}, {"name": "slide.rename", "args": {"name": "Copy"}}, {"name": "inspect"}]}))
            .expect("run");
        assert_eq!(ok["steps"].as_array().expect("steps").len(), 3);
        assert_eq!(app.session.undo_depth(), before + 1, "three steps are one undo step");
        assert_eq!(app.session.active().expect("doc").history.undo.last().expect("step").label, "Duplicate and rename");
        // One Undo takes the whole run back.
        app.run("edit.undo", json!({})).expect("undo");
        assert_eq!(app.session.active().expect("doc").doc.slides.len(), 1);
    }

    #[test]
    fn a_failing_step_takes_back_the_steps_before_it() {
        let mut app = app();
        let slides = app.session.active().expect("doc").doc.slides.len();
        let e = app.run_command_batch(&json!({"steps": [{"name": "slide.duplicate"}, {"name": "slide.go", "args": {"index": 99}}]})).expect_err("fails");
        assert!(e.starts_with("step-failed"), "{e}");
        assert_eq!(app.session.active().expect("doc").doc.slides.len(), slides);
    }

    #[test]
    fn shapes_are_selected_by_name_and_inspected() {
        let mut app = app();
        let info = app.run_command_batch(&json!({"steps": [{"name": "inspect"}]})).expect("inspect");
        let shapes = info["steps"][0]["result"]["shapes"].as_array().expect("shapes").clone();
        assert!(!shapes.is_empty());
        let name = shapes[0]["name"].as_str().expect("name").to_owned();
        app.run_command_batch(&json!({"steps": [{"name": "select.shapes", "args": {"names": [name]}}, {"name": "format.bold"}]})).expect("bold");
        let e = app.run_command_batch(&json!({"steps": [{"name": "select.shapes", "args": {"names": ["No such shape"]}}]})).expect_err("unknown name");
        assert!(e.contains("no shape named"), "{e}");
    }
}
