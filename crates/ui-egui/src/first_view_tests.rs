//! OneOS progressive open: the editor opens at the slide and object the host's read-only first view showed (the slide pane follows the
//! current slide), and that is not an edit.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use deckcraft_engine::Session;
use egui_kittest::Harness;
use serde_json::json;

use crate::{Services, SlideApp};

fn harness() -> Harness<'static, SlideApp> {
    let mut s = Session::with_new();
    for i in 0..14 {
        if i > 0 {
            s.execute("slide.new", &json!({})).unwrap();
        }
        s.execute("insert.textBox", &json!({"rect": [60, 60, 300, 40], "text": format!("Heading {}", i + 1)})).unwrap();
    }
    s.execute("slide.go", &json!({"index": 0})).unwrap();
    let mut h = Harness::builder().with_size(egui::vec2(1500.0, 800.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        |ui, app: &mut SlideApp| {
            let ctx = ui.ctx().clone();
            app.logic(&ctx);
            app.ui(ui);
        },
        SlideApp::new(s, Services::default()),
    );
    h.run_steps(6);
    h
}

#[test]
fn the_editor_opens_at_the_first_views_slide_and_object() {
    let mut h = harness();
    assert_eq!(h.state().ui.thumbs_scroll, 0.0, "the slide pane starts at the top");
    let undo = h.state().session.doc().unwrap().history.undo.len();
    assert!(h.state_mut().open_at_first_view("Heading 12", 0.0));
    h.run_steps(8);
    let d = h.state().session.doc().unwrap();
    assert_eq!(d.selection.slide, 11, "the slide holding the selected text is current");
    assert_eq!(d.selection.shapes.len(), 1, "and the object holding it is selected");
    assert_eq!(d.history.undo.len(), undo, "opening at a place is not an edit");
    assert!(h.state().ui.thumbs_scroll > 0.0, "the slide pane follows the current slide: {}", h.state().ui.thumbs_scroll);
}

#[test]
fn with_no_selection_the_slide_is_the_scroll_fraction_and_nothing_is_selected() {
    let mut h = harness();
    assert!(h.state_mut().open_at_first_view("", 1.0));
    h.run_steps(4);
    let d = h.state().session.doc().unwrap();
    assert_eq!(d.selection.slide, 13);
    assert!(d.selection.shapes.is_empty());
}
