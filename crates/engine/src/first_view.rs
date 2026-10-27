//! Where the host's read-only first view was left, as a place in the deck (OneOS progressive open, 2026-10-27).
//!
//! The viewer showed the slides as a scrolling column; the host says how far down it was (a fraction of the scrollable height) and which
//! text, if any, was selected. The editor opens at the same SLIDE and OBJECT: the slide at that fraction of the deck is the current one,
//! and the object whose text is the selected text (on the slide nearest that fraction) is selected on it. Pure: the deck in, a slide and
//! an object out; [`open_at_first_view`] puts the session there (not an edit: both commands are not undo steps).

use deckcraft_model::{Presentation, Shape};
use serde_json::json;

use crate::{Result, Session};

/// The first view as a slide and, when the selected text is an object's, that object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FirstViewPlace {
    pub slide: usize,
    pub shape: Option<u32>,
}

/// The shape (or a shape inside a group) whose text, trimmed, is `wanted` or contains it as a whole line.
fn find(shapes: &[Shape], wanted: &str) -> Option<u32> {
    for sh in shapes {
        if let Some(body) = &sh.text {
            let text = body.text();
            if text.trim() == wanted || text.lines().any(|l| l.trim() == wanted) {
                return Some(sh.id.0);
            }
        }
        if let deckcraft_model::ShapeKind::Group { children, .. } = &sh.kind
            && let Some(id) = find(children, wanted)
        {
            return Some(id);
        }
    }
    None
}

/// The place in `doc` for a first view scrolled to `y_fraction` (0 = top, 1 = end of the column of slides) with `selection` selected.
/// `None` for a deck with no slides.
pub fn locate(doc: &Presentation, selection: &str, y_fraction: f64) -> Option<FirstViewPlace> {
    let n = doc.slides.len();
    if n == 0 {
        return None;
    }
    let f = if y_fraction.is_finite() { y_fraction.clamp(0.0, 1.0) } else { 0.0 };
    let at = (((n - 1) as f64) * f).round() as usize;
    let wanted = selection.lines().map(str::trim).find(|l| !l.is_empty());
    if let Some(wanted) = wanted {
        // The slide nearest the scroll position that has the text (ties go to the earlier slide).
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by_key(|i| (i.abs_diff(at), *i));
        for i in order {
            if let Some(slide) = doc.slides.get(i)
                && let Some(id) = find(&slide.shapes, wanted)
            {
                return Some(FirstViewPlace { slide: i, shape: Some(id) });
            }
        }
    }
    Some(FirstViewPlace { slide: at, shape: None })
}

/// Put the active presentation at the first view's place: that slide is current, and that object selected on it. Nothing is undoable
/// (selection only). False when there is no deck to open at.
pub fn open_at_first_view(s: &mut Session, selection: &str, y_fraction: f64) -> Result<bool> {
    let Some(place) = locate(&s.doc()?.doc, selection, y_fraction) else { return Ok(false) };
    s.execute("slide.go", &json!({"index": place.slide}))?;
    if let Some(id) = place.shape {
        s.execute("edit.select", &json!({"ids": [id]}))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deck of ten slides, each with a text box "Slide N" (a title) and, on slide 7, a second box "Budget".
    fn deck() -> Session {
        let mut s = Session::with_new();
        for i in 0..10 {
            if i > 0 {
                s.execute("slide.new", &json!({})).unwrap();
            }
            s.execute("insert.textBox", &json!({"rect": [50, 50, 300, 40], "text": format!("Slide {}", i + 1)})).unwrap();
            if i == 6 {
                s.execute("insert.textBox", &json!({"rect": [50, 150, 300, 40], "text": "Budget"})).unwrap();
            }
        }
        s
    }

    #[test]
    fn the_slide_is_the_fraction_of_the_deck() {
        let s = deck();
        let d = &s.doc().unwrap().doc;
        assert_eq!(locate(d, "", 0.0).map(|p| p.slide), Some(0));
        assert_eq!(locate(d, "", 1.0).map(|p| p.slide), Some(9));
        assert_eq!(locate(d, "", 0.5).map(|p| p.slide), Some(5), "4.5 rounds to the later slide");
        assert_eq!(locate(d, "", f64::NAN).map(|p| p.slide), Some(0), "hostile numbers are the first slide");
    }

    #[test]
    fn selected_text_is_the_object_on_the_slide_nearest_the_scroll_position() {
        let s = deck();
        let d = &s.doc().unwrap().doc;
        let p = locate(d, "Budget", 0.0).unwrap();
        assert_eq!((p.slide, p.shape.is_some()), (6, true), "found on slide 7 wherever the view was");
        let title = locate(d, "Slide 3", 1.0).unwrap();
        assert_eq!(title.slide, 2);
        assert_eq!(locate(d, "not on any slide", 0.3).map(|p| (p.slide, p.shape)), Some((3, None)));
    }

    #[test]
    fn opening_at_the_place_makes_the_slide_current_and_selects_the_object_without_an_undo_step() {
        let mut s = deck();
        let undo_before = s.doc().unwrap().history.undo.len();
        let before = s.doc().unwrap().doc.clone();
        assert!(open_at_first_view(&mut s, "Budget", 0.0).unwrap());
        let sel = &s.doc().unwrap().selection;
        assert_eq!(sel.slide, 6);
        assert_eq!(sel.shapes.len(), 1);
        assert_eq!(s.doc().unwrap().history.undo.len(), undo_before, "selection only");
        assert_eq!(s.doc().unwrap().doc, before);
        assert!(open_at_first_view(&mut s, "", 1.0).unwrap());
        assert_eq!(s.doc().unwrap().selection.slide, 9);
    }
}
