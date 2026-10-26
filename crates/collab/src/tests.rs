use super::*;
use deckcraft_model::{Shape, ShapeId, Slide, SlideId, TextBody};

fn shape(id: u32, text: &str) -> Shape {
    Shape { id: ShapeId(id), name: format!("Box {id}"), text: Some(TextBody::from_text(text)), ..Default::default() }
}

fn deck() -> Presentation {
    let mut d = Presentation::default();
    d.slides = vec![
        Arc::new(Slide { id: SlideId(2), shapes: vec![shape(10, "Title\nsecond line"), shape(11, "Body")], ..Default::default() }),
        Arc::new(Slide { id: SlideId(3), shapes: vec![shape(12, "Other slide")], ..Default::default() }),
    ];
    d.next_id = 40;
    d
}

fn texts(d: &Presentation) -> Vec<Vec<String>> {
    d.slides.iter().map(|s| s.shapes.iter().map(|x| x.text.as_ref().map(TextBody::text).unwrap_or_default()).collect()).collect()
}

/// Seed `a`'s deck and have a second window read it.
fn joined(a: &mut Shim, deck_a: &Presentation) -> (Shim, Presentation) {
    a.seed(deck_a, 1_000).expect("seed");
    let mut b = Shim::new(77);
    let mut deck_b = Presentation::default();
    b.remote_state(a.view(), &mut deck_b).expect("state");
    (b, deck_b)
}

#[test]
fn a_seeded_deck_reads_back_whole_and_a_second_window_has_nothing_to_send() {
    let da = deck();
    let mut a = Shim::new(11);
    let (mut b, db) = joined(&mut a, &da);
    assert_eq!(texts(&db), texts(&da));
    assert_eq!(db.slides.len(), 2);
    assert_eq!(db.slides[0].shapes[0].name, "Box 10");
    assert!(b.local(&db, 2_000).expect("local").is_empty(), "reading is not an edit");
    assert!(a.local(&da, 2_000).expect("local").is_empty(), "the seeding window is in step too");
}

#[test]
fn typing_is_a_text_insert_only_and_joins_the_burst() {
    let mut da = deck();
    let mut a = Shim::new(11);
    a.seed(&da, 1_000).expect("seed");
    let mut s = (*da.slides[0]).clone();
    s.shapes[1].text = Some(TextBody::from_text("Body!"));
    da.slides[0] = Arc::new(s);
    let batches = a.local(&da, 1_100).expect("local");
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].label, "Typing");
    assert!(batches[0].ops.iter().all(|o| o["op"] == "text.insert"), "{:?}", batches[0].ops);
    let mut s = (*da.slides[0]).clone();
    s.shapes[1].text = Some(TextBody::from_text("Body!!"));
    da.slides[0] = Arc::new(s);
    let more = a.local(&da, 1_300).expect("local");
    assert!(more[0].join, "a burst folds into the previous undo step");
}

#[test]
fn a_moved_slide_and_a_deleted_shape_keep_the_rest_in_place() {
    let mut da = deck();
    let mut a = Shim::new(11);
    let (mut b, mut db) = joined(&mut a, &da);
    da.slides.swap(0, 1);
    let mut s = (*da.slides[1]).clone();
    s.shapes.remove(0);
    da.slides[1] = Arc::new(s);
    let batches = a.local(&da, 2_000).expect("local");
    assert!(!batches.is_empty());
    // The other window reads the new state.
    assert!(b.remote_state(a.view(), &mut db).expect("state"));
    assert_eq!(texts(&db), texts(&da));
}

#[test]
fn formatting_is_marks_and_paragraph_level_is_a_value() {
    let mut da = deck();
    let mut a = Shim::new(11);
    let (mut b, mut db) = joined(&mut a, &da);
    let mut s = (*da.slides[0]).clone();
    if let Some(t) = s.shapes[1].text.as_mut() {
        let p = &mut t.paragraphs[0];
        p.level = 2;
        p.runs = vec![Run::with("Bo", RunProps { bold: Some(true), size: Some(20.0), ..Default::default() }), Run::new("dy")];
    }
    da.slides[0] = Arc::new(s);
    let batches = a.local(&da, 2_000).expect("local");
    assert!(batches.iter().flat_map(|b| &b.ops).any(|o| o["op"] == "text.mark"), "{batches:?}");
    b.remote_state(a.view(), &mut db).expect("state");
    let p = &db.slides[0].shapes[1].text.as_ref().expect("text").paragraphs[0];
    assert_eq!(p.level, 2);
    assert_eq!(p.runs.len(), 2);
    assert_eq!(p.runs[0].props.bold, Some(true));
    assert_eq!(p.runs[0].props.size, Some(20.0));
    assert_eq!(p.runs[0].text, "Bo");
    assert!(b.local(&db, 3_000).expect("local").is_empty(), "what was read back is not re-sent");
}

#[test]
fn a_group_nests_by_keys_and_a_connector_names_a_shape_by_its_shared_key() {
    use deckcraft_model::Xfrm;
    let mut da = deck();
    let mut a = Shim::new(11);
    let group = Shape { id: ShapeId(20), kind: ShapeKind::Group { children: vec![shape(21, "in the group")], child: Xfrm::default() }, ..Default::default() };
    let connector = Shape { id: ShapeId(22), kind: ShapeKind::Connector { start: Some((ShapeId(20), 1)), end: Some((ShapeId(11), 2)) }, ..Default::default() };
    let mut s = (*da.slides[0]).clone();
    s.shapes.push(group);
    s.shapes.push(connector);
    da.slides[0] = Arc::new(s);
    let (_, db) = joined(&mut a, &da);
    let shapes = &db.slides[0].shapes;
    assert!(matches!(&shapes[2].kind, ShapeKind::Group { children, .. } if children.len() == 1 && children[0].text.as_ref().map(TextBody::text).as_deref() == Some("in the group")));
    // The connector's ends are this window's ids for the same shapes.
    let ShapeKind::Connector { start, end } = &shapes[3].kind else { panic!("connector") };
    assert_eq!(start.map(|(id, _)| id), Some(shapes[2].id));
    assert_eq!(end.map(|(id, _)| id), Some(shapes[1].id));
}

#[test]
fn a_window_that_meets_a_new_key_makes_ids_that_do_not_collide() {
    let da = deck();
    let mut a = Shim::new(11);
    let (_, db) = joined(&mut a, &da);
    let mut seen = std::collections::BTreeSet::new();
    for s in &db.slides {
        assert!(seen.insert(s.id.0), "slide ids differ");
        for x in &s.shapes {
            assert!(seen.insert(x.id.0), "shape ids differ from every other id: {}", x.id);
        }
    }
    assert!(db.next_id >= *seen.iter().max().expect("ids"), "the deck's counter is past every id made");
}

#[test]
fn pictures_travel_as_content_addresses_and_wait_as_placeholders() {
    use deckcraft_model::{MediaId, MediaItem};
    let bytes = Arc::new(b"not really a png".to_vec());
    let hash = media::sha256_hex(&bytes);
    let mut da = deck();
    da.media.push(MediaItem { id: MediaId(30), name: "a.png".into(), content_type: "image/png".into(), data: bytes.clone(), link: None });
    let pic = Shape {
        id: ShapeId(31),
        kind: ShapeKind::Picture { fill: deckcraft_model::style::PictureFill { media: MediaId(30), ..Default::default() } },
        ..Default::default()
    };
    let mut s = (*da.slides[0]).clone();
    s.shapes.push(pic);
    da.slides[0] = Arc::new(s);
    let mut a = Shim::new(11);
    let batches = a.seed(&da, 1_000).expect("seed");
    let sent = batches.iter().flat_map(|b| &b.ops).map(Value::to_string).collect::<String>();
    assert!(sent.contains(&format!("cas-{hash}")), "the picture goes by its content address");
    assert!(!sent.contains("not really a png"), "the bytes never go");
    let mut b = Shim::new(77);
    let mut db = Presentation::default();
    b.remote_state(a.view(), &mut db).expect("state");
    assert_eq!(b.take_shown().wanted, vec![hash.clone()], "the window asks for the bytes it lacks");
    let ShapeKind::Picture { fill } = &db.slides[0].shapes[2].kind else { panic!("picture") };
    let media_id = fill.media;
    let item = db.media.iter().find(|m| m.id == media_id).expect("a placeholder stands in");
    assert!(item.data.is_empty());
    // The bytes arrive: the placeholder is filled, and a wrong answer is not believed.
    let real = MediaItem { id: MediaId(1), name: "a.png".into(), content_type: "image/png".into(), data: bytes, link: None };
    let wrong = MediaItem { data: Arc::new(b"other".to_vec()), ..real.clone() };
    assert!(!b.media_arrived(&mut db, &hash, &wrong));
    assert!(b.media_arrived(&mut db, &hash, &real));
    assert_eq!(db.media.iter().find(|m| m.id == media_id).expect("item").data.len(), 16);
    assert!(b.take_shown().wanted.is_empty());
    assert!(b.local(&db, 2_000).expect("local").is_empty());
}

#[test]
fn line_breaks_and_fields_are_inline_objects_that_travel_with_their_character() {
    use deckcraft_model::text::RunKind;
    let mut da = deck();
    let mut s = (*da.slides[0]).clone();
    if let Some(t) = s.shapes[1].text.as_mut() {
        t.paragraphs[0].runs = vec![
            Run::new("page "),
            Run { text: "3".into(), props: RunProps { bold: Some(true), ..Default::default() }, kind: RunKind::Field { field: "slidenum".into() } },
            Run::new(" of x"),
            Run { text: String::new(), props: RunProps::default(), kind: RunKind::Break },
            Run::new("next line"),
        ];
    }
    da.slides[0] = Arc::new(s);
    let mut a = Shim::new(11);
    let (mut b, db) = joined(&mut a, &da);
    let p = &db.slides[0].shapes[1].text.as_ref().expect("text").paragraphs[0];
    assert_eq!(p.runs.len(), 5, "{p:?}");
    assert!(matches!(&p.runs[1].kind, RunKind::Field { field } if field == "slidenum") && p.runs[1].text == "3");
    assert!(matches!(p.runs[3].kind, RunKind::Break));
    assert_eq!(p.runs[4].text, "next line");
    assert!(b.local(&db, 2_000).expect("local").is_empty());
    assert_eq!(a.digest(&da).expect("digest"), b.digest(&db).expect("digest"));
}

#[test]
fn numbers_in_float_or_integer_spelling_are_one_value() {
    let mut da = deck();
    let mut s = (*da.slides[0]).clone();
    s.shapes[0].xfrm = Some(deckcraft_model::Xfrm { x: 20.0, y: 30.5, w: 400.0, h: 80.0, ..Default::default() });
    da.slides[0] = Arc::new(s);
    let mut a = Shim::new(11);
    let (mut b, db) = joined(&mut a, &da);
    assert_eq!(db.slides[0].shapes[0].xfrm, da.slides[0].shapes[0].xfrm);
    assert!(b.local(&db, 2_000).expect("local").is_empty(), "20 and 20.0 are one value");
}
