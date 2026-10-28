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

/// Every command the app has and the assistant may NOT run, with the reason: together with `ASSISTANT_COMMANDS` and `ARGUMENT_COMMANDS` this is the whole command
/// catalog (a test holds the three to it, so a command can neither be forgotten nor listed twice).
pub const LEFT_OUT: &[(&str, &[&str])] = &[
    (
        "Files and addresses move only through the host: open, save, export, print, close, recover and the like are the host's, never a command id the assistant runs; so are the commands that read a file, a picture, a sound, a video, a font or a web address.",
        &[
            "file.new",
            "file.templates",
            "file.open",
            "file.adopt",
            "file.openBytes",
            "file.save",
            "file.saveAs",
            "file.saveTemplate",
            "file.saveBytes",
            "file.export",
            "file.info",
            "file.render",
            "file.close",
            "file.properties",
            "file.recovery.save",
            "file.recovery.list",
            "file.recovery.open",
            "file.recovery.discard",
            "file.revert",
            "slide.fromOutline",
            "slide.reuseList",
            "slide.reuse",
            "insert.icon",
            "insert.icons",
            "insert.picture",
            "insert.photoAlbum",
            "insert.photoAlbumEdit",
            "insert.audio",
            "insert.video",
            "insert.hyperlink",
            "picture.change",
            "font.add",
            "media.posterFrame",
        ],
    ),
    ("Undo, redo and the history belong to the person: a run is one undo step, and undoing it is theirs.", &["edit.undo", "edit.redo"]),
    (
        "The system clipboard is the person's (copy, cut and paste read or write what they hold outside the app), and so are the format painters.",
        &[
            "edit.cut",
            "edit.copy",
            "edit.paste",
            "edit.pasteSpecial",
            "edit.pasteText",
            "format.painter",
            "format.painterApply",
            "animation.painter",
            "animation.painterApply",
        ],
    ),
    (
        "The window, the slide show, the preferences, the add-ins and the help are the person's, not the document's: new windows, running a show, updates and about, the command list.",
        &[
            "window.next",
            "font.rescan",
            "view.slideMaster",
            "view.handoutMaster",
            "view.notesMaster",
            "view.closeMaster",
            "show.fromStart",
            "show.fromCurrent",
            "show.setup",
            "show.customShow",
            "commands.list",
        ],
    ),
    (
        "Interactive: it begins a pointer gesture or an editing session (editing a path's points, drawing, selecting text in place), which only a person's pointer or keyboard can finish.",
        &[
            "shape.freeform",
            "draw.inkToShape",
            "shape.editPoints",
            "shape.points",
            "shape.movePoint",
            "shape.addPoint",
            "shape.pointType",
            "shape.moveHandle",
            "shape.deletePoint",
            "shape.deleteSegment",
            "shape.segmentType",
            "shape.openPath",
            "shape.closePath",
            "text.edit",
            "text.exit",
            "text.compose",
            "text.commit",
            "text.move",
            "text.select",
            "text.selectAll",
            "text.selectWord",
            "text.selectParagraph",
            "format.fontDialog",
            "format.paragraphDialog",
            "table.drawLine",
            "table.eraseLine",
            "table.selectCells",
        ],
    ),
    (
        "Takes arguments whose shape has not yet been checked against a run (a shape, a range, a name, a setting), so the assistant does not run it yet; every command that acts at once on the selection is in the list.",
        &[
            "slide.colorAt",
            "slide.next",
            "slide.previous",
            "slide.first",
            "slide.last",
            "slide.selectSlides",
            "slide.notes",
            "section.remove",
            "shape.insert",
            "shape.presets",
            "insert.table",
            "insert.chart",
            "insert.wordArt",
            "insert.slideNumber",
            "insert.dateTime",
            "insert.symbol",
            "shape.action",
            "chart.type",
            "chart.data",
            "chart.options",
            "insert.smartArt",
            "insert.actionButton",
            "smartArt.nodes",
            "smartArt.setNodes",
            "smartArt.addNode",
            "smartArt.removeNode",
            "smartArt.promote",
            "smartArt.demote",
            "smartArt.layout",
            "smartArt.fromText",
            "smartArt.toText",
            "smartArt.toShapes",
            "smartArt.picture",
            "smartArt.style",
            "smartArt.colors",
            "shape.connect",
            "shape.sites",
            "shape.move",
            "shape.rotate",
            "shape.sizePosition",
            "shape.flip",
            "shape.adjust",
            "shape.change",
            "shape.smoothCorners",
            "shape.fill",
            "shape.line",
            "shape.effects",
            "shape.quickStyle",
            "shape.lock",
            "shape.setDefault",
            "picture.crop",
            "picture.adjust",
            "picture.reset",
            "media.options",
            "picture.removeBackground",
            "picture.style",
            "picture.styles",
            "picture.recolor",
            "picture.recolors",
            "picture.transparentColor",
            "guide.add",
            "guide.move",
            "guide.delete",
            "guide.list",
            "arrange.reorder",
            "arrange.regroup",
            "arrange.align",
            "arrange.distribute",
            "selection.select",
            "selection.showAll",
            "selection.hideAll",
            "shape.merge",
            "font.used",
            "font.replace",
            "text.autocorrect",
            "text.delete",
            "text.set",
            "text.get",
            "format.underline",
            "format.strikethrough",
            "format.superscript",
            "format.subscript",
            "format.grow",
            "format.shrink",
            "format.textTransparency",
            "format.language",
            "format.highlight",
            "format.case",
            "format.caps",
            "format.spacing",
            "format.textShadow",
            "format.textOutline",
            "format.textWarp",
            "format.align",
            "format.rtl",
            "format.bullets",
            "format.numbering",
            "format.lineSpacing",
            "format.paragraph",
            "format.ruler",
            "format.tabs",
            "format.direction",
            "format.columns",
            "format.autofit",
            "format.wrap",
            "format.margins",
            "format.state",
            "design.browseTheme",
            "design.themes",
            "design.colors",
            "design.variants",
            "design.fonts",
            "design.background",
            "design.slideSize",
            "design.headerFooter",
            "handout.placeholders",
            "notes.placeholders",
            "master.insertLayout",
            "master.renameLayout",
            "master.deleteLayout",
            "master.insertPlaceholder",
            "master.preserve",
            "master.placeholders",
            "design.ideas",
            "design.applyIdea",
            "transition.set",
            "transition.timing",
            "transition.sound",
            "transition.list",
            "animation.set",
            "animation.add",
            "animation.remove",
            "animation.move",
            "animation.timing",
            "animation.options",
            "animation.clear",
            "animation.list",
            "animation.get",
            "table.split",
            "table.style",
            "table.options",
            "table.cellFill",
            "table.borders",
            "table.cellMargins",
            "table.cellAlign",
            "table.columnWidth",
            "text.find",
            "text.replace",
            "comment.list",
            "comment.next",
            "comment.prev",
            "review.accessibility",
            "review.spelling",
            "review.spellingChange",
            "review.addToDictionary",
            "review.dictionary",
            "review.thesaurus",
            "review.replaceWord",
            "review.compare",
            "review.revisions",
            "review.accept",
            "review.reject",
            "review.endCompare",
            "document.inspect",
            "slide.inspect",
            "shape.inspect",
            "media.play",
            "media.pause",
            "media.toggle",
            "media.stop",
            "media.seek",
            "media.info",
            "media.gifPlay",
            "picture.artisticEffect",
            "insert.zoom",
            "picture.compress",
        ],
    ),
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
            assert!(
                !id.contains("file")
                    && !id.contains("path")
                    && !id.contains("open")
                    && !id.contains("save")
                    && !id.contains("export")
                    && !id.contains("import"),
                "{id}"
            );
        }
    }

    #[test]
    fn a_run_is_judged_whole_first_and_is_one_undo_step() {
        let mut app = app();
        let before = app.session.undo_depth();
        let slides = app.session.active().expect("doc").doc.slides.len();
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
        assert_eq!(app.session.active().expect("doc").doc.slides.len(), slides);
    }

    #[test]
    fn a_failing_step_takes_back_the_steps_before_it() {
        let mut app = app();
        let slides = app.session.active().expect("doc").doc.slides.len();
        let e =
            app.run_command_batch(&json!({"steps": [{"name": "slide.duplicate"}, {"name": "slide.go", "args": {"index": 99}}]})).expect_err("fails");
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
        let e =
            app.run_command_batch(&json!({"steps": [{"name": "select.shapes", "args": {"names": ["No such shape"]}}]})).expect_err("unknown name");
        assert!(e.contains("no shape named"), "{e}");
    }

    #[test]
    fn the_allow_list_and_the_commands_left_out_are_the_whole_catalog_with_nothing_twice() {
        let all: Vec<String> = deckcraft_engine::Session::new().commands().iter().map(|c| c.id.to_string()).collect();
        let mut seen = std::collections::HashSet::new();
        for id in ASSISTANT_COMMANDS {
            assert!(all.iter().any(|c| c == id), "{id} is not a command of the app");
            assert!(seen.insert(id.to_string()), "{id} is listed twice");
        }
        // The window's own commands stand beside the registry's: they are not in it.
        for id in ARGUMENT_COMMANDS {
            assert!(seen.insert(id.to_string()), "{id} is listed twice");
        }
        for (reason, ids) in LEFT_OUT {
            assert!(reason.len() > 20, "a reason: {reason}");
            for id in *ids {
                assert!(all.iter().any(|c| c == id), "{id} is left out but is not a command of the app");
                assert!(seen.insert(id.to_string()), "{id} is both allowed and left out, or left out twice");
            }
        }
        let missing: Vec<&String> = all.iter().filter(|c| !seen.contains(c.as_str())).collect();
        assert!(missing.is_empty(), "commands neither allowed nor left out (with a reason): {missing:?}");
    }
}
