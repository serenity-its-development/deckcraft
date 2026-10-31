//! OpenDocument Presentation (`.odp`) import, written from the public OASIS ODF 1.2/1.3
//! specification.
//!
//! Reads `content.xml` (pages, frames, text, drawing shapes, tables, pictures, notes), `styles.xml`
//! (page size, master backgrounds, named styles, fill images and gradients) and `meta.xml` into the
//! DeckCraft model. Like the PPTX reader it is lenient and bounded: anything it doesn't model is
//! skipped, every size and depth is capped. Groups are flattened (ODF stores absolute positions),
//! custom-shape geometry maps to the matching preset or to its own path, and pictures in formats
//! nothing here can draw (StarView `.svm` previews) are left out.

use std::collections::HashMap;
use std::io::Read as _;
use std::sync::Arc;

use deckcraft_color::Rgba;
use deckcraft_geom::{Size, Xfrm};
use deckcraft_model::style::{ColorRef, Dash, Fill, Gradient, GradientShape, GradientStop, Line, PictureFill, PictureMode};
use deckcraft_model::table::{Cell, Row, Table};
use deckcraft_model::text::{Align, Anchor, BodyProps, Bullet, ParaProps, Paragraph, Run, RunKind, RunProps, Spacing, Strike, TextBody};
use deckcraft_model::{
    Background, CustomPath, Geom, LayoutId, LayoutType, MediaId, PhType, Placeholder, Presentation, Shape, ShapeId, ShapeKind, Slide, SlideId,
    defaults,
};

use crate::PptxError;
use crate::opc::Package;
use crate::xml::{Doc, El, Node};

/// The media types an ODF package's `mimetype` names for a presentation (and its template).
const MIME_ODP: &str = "application/vnd.oasis.opendocument.presentation";
/// Nesting of groups, lists and spans followed.
const MAX_NEST: usize = 64;
/// Shapes read per page (a hostile file can't make a page arbitrarily large).
const MAX_SHAPES_PER_PAGE: usize = 20_000;
/// Pages read.
const MAX_PAGES: usize = 10_000;
/// Table cells read per table.
const MAX_CELLS: usize = 100_000;

/// Is this an ODF presentation package (`mimetype` says `…opendocument.presentation[-template]`)?
pub fn sniff(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"PK") {
        return false;
    }
    let Ok(mut z) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else { return false };
    let Ok(f) = z.by_name("mimetype") else { return false };
    let mut s = String::new();
    if f.take(128).read_to_string(&mut s).is_err() {
        return false;
    }
    s.trim().starts_with(MIME_ODP)
}

/// Read an `.odp` / `.otp` package.
pub fn import(bytes: &[u8]) -> Result<Presentation, PptxError> {
    let pkg = Package::open(bytes)?;
    let content = pkg.xml("content.xml").ok_or_else(|| PptxError::NotPptx("an ODF presentation needs a readable content.xml".into()))?;
    let styles = pkg.xml("styles.xml").unwrap_or_default();
    let meta = pkg.xml("meta.xml");
    let body =
        content.root.path(&["body", "presentation"]).ok_or_else(|| PptxError::NotPptx("content.xml has no office:presentation body".into()))?;
    let st = Styles::new(&content, &styles);
    let pages: Vec<&El> = body.children_named("page").take(MAX_PAGES).collect();

    let size = pages.first().and_then(|pg| st.page_size(pg.attr("draw:master-page-name").unwrap_or(""))).unwrap_or(Size::new(793.7, 595.3));
    let mut p = defaults::blank_presentation(deckcraft_format_sane(size), Default::default(), false);
    if let Some(m) = meta {
        read_meta(&m.root, &mut p);
    }
    let mut imp = Imp { pkg: &pkg, st: &st, p: &mut p, media_by_part: HashMap::new() };
    for page in pages {
        if let Some(slide) = imp.page(page) {
            imp.p.slides.push(Arc::new(slide));
        }
    }
    p.fix_next_id();
    Ok(p)
}

/// Keep a page size the model can lay out (the PPTX reader's `sane_slide_size` lives in the format
/// crate, which depends on this one).
fn deckcraft_format_sane(s: Size) -> Size {
    let ok = |v: f64| v.is_finite() && (36.0..=4032.0).contains(&v);
    if ok(s.width) && ok(s.height) { s } else { defaults::WIDE }
}

fn read_meta(root: &El, p: &mut Presentation) {
    let Some(m) = root.child("meta") else { return };
    let get = |n: &str| m.child(n).map(|e| e.text().trim().to_string()).unwrap_or_default();
    p.props.title = get("title");
    p.props.subject = get("subject");
    p.props.author = get("initial-creator");
    p.props.last_modified_by = get("creator");
    p.props.created = get("creation-date");
    p.props.modified = get("date");
    p.props.comments = get("description");
    p.props.keywords = m.children_named("keyword").map(|k| k.text().trim().to_string()).collect::<Vec<_>>().join(", ");
}

// ------------------------------------------------------------------------------------------ units

/// An ODF length (`2.54cm`, `10mm`, `1in`, `12pt`, `1pc`, `16px`) in points.
fn len_pt(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic() || c == '%').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let v: f64 = num.trim().parse().ok()?;
    let pt = match unit.trim() {
        "cm" => v * 72.0 / 2.54,
        "mm" => v * 72.0 / 25.4,
        "in" | "inch" => v * 72.0,
        "pt" | "" => v,
        "pc" => v * 12.0,
        "px" => v * 0.75,
        _ => return None,
    };
    pt.is_finite().then_some(pt)
}

fn attr_len(e: &El, name: &str) -> Option<f64> {
    e.attr(name).and_then(len_pt)
}

fn color(s: &str) -> Option<ColorRef> {
    Rgba::from_hex(s).map(ColorRef::rgb)
}

// ----------------------------------------------------------------------------------------- styles

/// The style families an ODF document defines, looked up the way ODF inherits: the named style,
/// its parents (`style:parent-style-name`), then the family's `style:default-style`.
struct Styles<'a> {
    /// Automatic styles of `content.xml`, then the common styles of `styles.xml`.
    content: HashMap<&'a str, &'a El>,
    /// Automatic styles of `styles.xml` (master pages and page layouts use these).
    master_auto: HashMap<&'a str, &'a El>,
    common: HashMap<&'a str, &'a El>,
    defaults: HashMap<&'a str, &'a El>,
    master_pages: HashMap<&'a str, &'a El>,
    page_layouts: HashMap<&'a str, &'a El>,
    /// `draw:fill-image`, `draw:gradient` by `draw:name`.
    fill_images: HashMap<&'a str, &'a El>,
    gradients: HashMap<&'a str, &'a El>,
    /// `style:font-face` name → family.
    fonts: HashMap<&'a str, String>,
    /// `text:list-style` by name.
    list_styles: HashMap<&'a str, &'a El>,
}

impl<'a> Styles<'a> {
    fn new(content: &'a Doc, styles: &'a Doc) -> Self {
        let mut st = Styles {
            content: HashMap::new(),
            master_auto: HashMap::new(),
            common: HashMap::new(),
            defaults: HashMap::new(),
            master_pages: HashMap::new(),
            page_layouts: HashMap::new(),
            fill_images: HashMap::new(),
            gradients: HashMap::new(),
            fonts: HashMap::new(),
            list_styles: HashMap::new(),
        };
        let named = |e: &'a El, map: &mut HashMap<&'a str, &'a El>| {
            for s in e.elements().filter(|c| c.is("style")) {
                if let Some(n) = s.attr("style:name") {
                    map.insert(n, s);
                }
            }
        };
        for doc in [content, styles] {
            if let Some(ff) = doc.root.child("font-face-decls") {
                for f in ff.children_named("font-face") {
                    if let (Some(n), Some(fam)) = (f.attr("style:name"), f.attr("svg:font-family")) {
                        st.fonts.insert(n, fam.trim_matches(|c| c == '\'' || c == '"').to_string());
                    }
                }
            }
        }
        if let Some(a) = content.root.child("automatic-styles") {
            named(a, &mut st.content);
            for l in a.children_named("list-style") {
                if let Some(n) = l.attr("style:name") {
                    st.list_styles.insert(n, l);
                }
            }
        }
        if let Some(a) = styles.root.child("automatic-styles") {
            named(a, &mut st.master_auto);
            for l in a.children_named("page-layout") {
                if let Some(n) = l.attr("style:name") {
                    st.page_layouts.insert(n, l);
                }
            }
        }
        if let Some(c) = styles.root.child("styles") {
            named(c, &mut st.common);
            for e in c.elements() {
                match e.local() {
                    "default-style" => {
                        if let Some(f) = e.attr("style:family") {
                            st.defaults.insert(f, e);
                        }
                    }
                    "fill-image" => {
                        if let Some(n) = e.attr("draw:name") {
                            st.fill_images.insert(n, e);
                        }
                    }
                    "gradient" => {
                        if let Some(n) = e.attr("draw:name") {
                            st.gradients.insert(n, e);
                        }
                    }
                    "list-style" => {
                        if let Some(n) = e.attr("style:name") {
                            st.list_styles.entry(n).or_insert(e);
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(m) = styles.root.child("master-styles") {
            for mp in m.children_named("master-page") {
                if let Some(n) = mp.attr("style:name") {
                    st.master_pages.insert(n, mp);
                }
            }
        }
        st
    }

    fn lookup(&self, name: &str, master: bool) -> Option<&'a El> {
        if master {
            self.master_auto.get(name).or_else(|| self.common.get(name)).copied()
        } else {
            self.content.get(name).or_else(|| self.common.get(name)).copied()
        }
    }

    /// The value of `attr` on the `props` element (`graphic-properties`, `text-properties`…) of
    /// style `name` or the nearest ancestor that states it, then the family default.
    fn prop(&self, name: Option<&str>, props: &str, attr: &str, master: bool) -> Option<&'a str> {
        let mut cur = name.and_then(|n| self.lookup(n, master));
        let mut family = None;
        for _ in 0..32 {
            let Some(s) = cur else { break };
            family = family.or(s.attr("style:family"));
            if let Some(v) = s.elements().filter(|e| e.is(props)).find_map(|e| e.attr(attr)) {
                return Some(v);
            }
            cur = s.attr("style:parent-style-name").and_then(|n| self.lookup(n, master));
        }
        let d = self.defaults.get(family?)?;
        d.elements().filter(|e| e.is(props)).find_map(|e| e.attr(attr))
    }

    fn page_size(&self, master: &str) -> Option<Size> {
        let mp = self.master_pages.get(master).or_else(|| self.master_pages.values().next())?;
        let pl = self.page_layouts.get(mp.attr("style:page-layout-name")?)?;
        let pr = pl.child("page-layout-properties")?;
        Some(Size::new(attr_len(pr, "fo:page-width")?, attr_len(pr, "fo:page-height")?))
    }
}

// ----------------------------------------------------------------------------------------- import

struct Imp<'a, 'b> {
    pkg: &'a Package,
    st: &'a Styles<'a>,
    p: &'b mut Presentation,
    media_by_part: HashMap<String, MediaId>,
}

/// What a page holds of each placeholder class, to choose the slide's layout.
#[derive(Default)]
struct Classes {
    title: bool,
    subtitle: bool,
    outlines: usize,
}

impl Imp<'_, '_> {
    fn id(&mut self) -> Option<u32> {
        self.p.alloc_id()
    }

    fn page(&mut self, page: &El) -> Option<Slide> {
        let id = SlideId(self.id()?);
        let mut classes = Classes::default();
        for f in page.children_named("frame") {
            match f.attr("presentation:class") {
                Some("title") => classes.title = true,
                Some("subtitle") => classes.subtitle = true,
                Some("outline") => classes.outlines += 1,
                _ => {}
            }
        }
        let layout = self.layout_for(&classes);
        let mut slide = Slide { id, layout, ..Default::default() };
        if let Some(n) = page.attr("draw:name")
            && !is_generated_page_name(n)
        {
            slide.name = n.to_string();
        }
        slide.hidden = matches!(page.attr("presentation:visibility"), Some("hidden"));
        slide.background = self.page_background(page);
        let mut used_ph: Vec<(PhType, u32)> = vec![];
        let mut out = vec![];
        self.shapes(page, &mut out, &mut used_ph, layout, 0);
        slide.shapes = out;
        if let Some(notes) = page.child("notes") {
            for f in notes.children_named("frame") {
                if f.attr("presentation:class") == Some("notes")
                    && let Some(tb) = f.child("text-box")
                {
                    slide.notes = self.text_body(tb, f, false);
                }
            }
        }
        Some(slide)
    }

    fn layout_for(&self, c: &Classes) -> LayoutId {
        let kind = match (c.title, c.subtitle, c.outlines) {
            (_, true, _) => LayoutType::Title,
            (true, _, n) if n >= 2 => LayoutType::TwoContent,
            (_, _, n) if n >= 1 => LayoutType::TitleAndContent,
            (true, _, _) => LayoutType::TitleOnly,
            _ => LayoutType::Blank,
        };
        defaults::layout_of_kind(self.p, kind).or_else(|| self.p.masters.first().and_then(|m| m.layouts.first()).map(|l| l.id)).unwrap_or_default()
    }

    fn page_background(&mut self, page: &El) -> Option<Background> {
        let own = page.attr("draw:style-name");
        if let Some(f) = self.fill(own, "drawing-page-properties", false) {
            return Some(Background::Fill { fill: f });
        }
        let master = page.attr("draw:master-page-name").and_then(|m| self.st.master_pages.get(m).copied());
        let mstyle = master.and_then(|m| m.attr("draw:style-name"));
        self.fill(mstyle, "drawing-page-properties", true).map(|fill| Background::Fill { fill })
    }

    /// The fill a style states (`draw:fill` and its colour, gradient or image); `None` when no
    /// style in the chain says.
    fn fill(&mut self, style: Option<&str>, props: &str, master: bool) -> Option<Fill> {
        let st = self.st;
        let kind = st.prop(style, props, "draw:fill", master)?;
        match kind {
            "none" => Some(Fill::None),
            "solid" => Some(Fill::solid(
                st.prop(style, props, "draw:fill-color", master).and_then(color).unwrap_or(ColorRef::rgb(Rgba::rgb(0x72, 0x9f, 0xcf))),
            )),
            "gradient" => {
                let g = st.prop(style, props, "draw:fill-gradient-name", master).and_then(|n| st.gradients.get(n).copied())?;
                Some(gradient(g))
            }
            "bitmap" => {
                let img = st.prop(style, props, "draw:fill-image-name", master).and_then(|n| st.fill_images.get(n).copied())?;
                let media = self.media(img.attr("xlink:href")?)?;
                let repeat = st.prop(style, props, "style:repeat", master).unwrap_or("repeat");
                let mode = if repeat == "repeat" {
                    PictureMode::Tile { tx: 0.0, ty: 0.0, sx: 1.0, sy: 1.0, flip: String::new(), align: "tl".into() }
                } else {
                    PictureMode::default()
                };
                Some(Fill::Picture(PictureFill { media, mode, ..Default::default() }))
            }
            _ => None,
        }
    }

    fn line(&self, style: Option<&str>) -> Option<Line> {
        let st = self.st;
        let stroke = st.prop(style, "graphic-properties", "draw:stroke", false)?;
        if stroke == "none" {
            return Some(Line::none());
        }
        let c = st.prop(style, "graphic-properties", "svg:stroke-color", false).and_then(color).unwrap_or(ColorRef::rgb(Rgba::rgb(0x34, 0x65, 0xa4)));
        let w = st.prop(style, "graphic-properties", "svg:stroke-width", false).and_then(len_pt).unwrap_or(0.0).max(0.75);
        let mut l = Line::solid(c, w);
        if stroke == "dash" {
            l.dash = Some(Dash::Preset { name: "dash".into() });
        }
        Some(l)
    }

    /// A picture part as a media item (each part once).
    fn media(&mut self, href: &str) -> Option<MediaId> {
        let part = href.trim_start_matches("./");
        if part.contains("://") || !drawable_picture(part) {
            return None;
        }
        if let Some(id) = self.media_by_part.get(part) {
            return Some(*id);
        }
        let data = self.pkg.get(part)?.to_vec();
        let name = part.rsplit('/').next().unwrap_or(part).to_string();
        let id = self.p.add_media(&name, crate::mime_for(&name), data)?;
        self.media_by_part.insert(part.to_string(), id);
        Some(id)
    }

    /// The shapes under `parent` (a page or a group), flattened.
    fn shapes(&mut self, parent: &El, out: &mut Vec<Shape>, used: &mut Vec<(PhType, u32)>, layout: LayoutId, depth: usize) {
        if depth > MAX_NEST {
            return;
        }
        for e in parent.elements() {
            if out.len() >= MAX_SHAPES_PER_PAGE {
                return;
            }
            let shape = match e.local() {
                "g" => {
                    self.shapes(e, out, used, layout, depth + 1);
                    continue;
                }
                "frame" => self.frame(e, used, layout),
                "custom-shape" | "rect" | "ellipse" | "circle" | "caption" => self.drawn(e),
                "line" | "connector" => self.line_shape(e),
                "polygon" | "polyline" | "path" => self.poly(e),
                _ => continue,
            };
            if let Some(s) = shape {
                out.push(s);
            }
        }
    }

    fn base_shape(&mut self, e: &El) -> Option<Shape> {
        let id = ShapeId(self.id()?);
        let name = e.attr("draw:name").map(str::to_string).unwrap_or_else(|| format!("Shape {}", id.0));
        let mut s = Shape { id, name, xfrm: Some(xfrm(e)), kind: ShapeKind::Shape, ..Default::default() };
        s.descr = e.child("desc").map(|d| d.text()).unwrap_or_default();
        s.title = e.child("title").map(|d| d.text()).unwrap_or_default();
        Some(s)
    }

    fn frame(&mut self, e: &El, used: &mut Vec<(PhType, u32)>, layout: LayoutId) -> Option<Shape> {
        let class = e.attr("presentation:class");
        if matches!(class, Some("footer" | "date-time" | "page-number" | "header")) {
            // Footer-row text belongs to the master and the deck's header & footer settings.
            return None;
        }
        let mut s = self.base_shape(e)?;
        let gstyle = e.attr("draw:style-name");
        // A table first (its draw:image is only LibreOffice's preview of it).
        if let Some(t) = e.child("table") {
            let x = s.xfrm.unwrap_or_default();
            s.kind = ShapeKind::Table(self.table(t, x.w, x.h));
            return Some(s);
        }
        if let Some(tb) = e.child("text-box") {
            let ph = class.and_then(|c| self.placeholder(c, used, layout));
            s.text = Some(self.text_body(tb, e, ph.is_none()));
            if let Some(ph) = ph {
                s.ph = Some(ph);
                if let Some(f) = self.fill(e.attr("presentation:style-name"), "graphic-properties", false)
                    && !f.is_none()
                {
                    s.fill = Some(f);
                }
            } else {
                s.text_box = true;
                s.fill = Some(self.fill(gstyle, "graphic-properties", false).unwrap_or(Fill::None));
                s.line = Some(self.line(gstyle).unwrap_or_else(Line::none));
            }
            return Some(s);
        }
        // A picture: the first draw:image in a format we can draw.
        let hrefs: Vec<String> = e.children_named("image").filter_map(|i| i.attr("xlink:href")).map(str::to_string).collect();
        let media = hrefs.iter().find_map(|h| self.media(h))?;
        s.kind = ShapeKind::Picture { fill: PictureFill { media, ..Default::default() } };
        s.line = Some(self.line(gstyle).unwrap_or_else(Line::none));
        Some(s)
    }

    /// The placeholder an ODF presentation class becomes on the slide's layout (each layout
    /// placeholder used once; extra outlines get their own index).
    fn placeholder(&self, class: &str, used: &mut Vec<(PhType, u32)>, layout: LayoutId) -> Option<Placeholder> {
        let want: &[PhType] = match class {
            "title" => &[PhType::Title, PhType::CtrTitle],
            "subtitle" => &[PhType::SubTitle, PhType::Body],
            "outline" | "text" => &[PhType::Body, PhType::Obj],
            _ => return None,
        };
        let lay = self.p.layout(layout).map(|(_, l)| l.shapes.clone()).unwrap_or_default();
        let found = lay.iter().filter_map(|s| s.ph.clone()).find(|ph| want.contains(&ph.kind) && !used.contains(&(ph.kind, ph.idx)));
        let ph = found.unwrap_or_else(|| {
            let kind = want.first().copied().unwrap_or_default();
            let idx = if kind.is_title() { 0 } else { 100 + u32::try_from(used.len()).unwrap_or(0) };
            Placeholder { kind, idx, ..Default::default() }
        });
        used.push((ph.kind, ph.idx));
        Some(ph)
    }

    /// A drawing shape with optional text: custom shapes, rectangles, ellipses, captions.
    fn drawn(&mut self, e: &El) -> Option<Shape> {
        let mut s = self.base_shape(e)?;
        let style = e.attr("draw:style-name");
        s.geom = match e.local() {
            "ellipse" | "circle" => Geom::preset("ellipse"),
            "rect" if e.attr("draw:corner-radius").and_then(len_pt).is_some_and(|r| r > 0.0) => Geom::preset("roundRect"),
            "custom-shape" => e.child("enhanced-geometry").map(geometry).unwrap_or_default(),
            _ => Geom::default(),
        };
        if let Some(g) = e.child("enhanced-geometry")
            && let Some(x) = s.xfrm.as_mut()
        {
            x.flip_h ^= g.attr("draw:mirror-horizontal") == Some("true");
            x.flip_v ^= g.attr("draw:mirror-vertical") == Some("true");
        }
        s.fill = Some(self.fill(style, "graphic-properties", false).unwrap_or(Fill::solid(ColorRef::rgb(Rgba::rgb(0x72, 0x9f, 0xcf)))));
        s.line = Some(self.line(style).unwrap_or_else(|| Line::solid(ColorRef::rgb(Rgba::rgb(0x34, 0x65, 0xa4)), 0.75)));
        let tb = self.text_body(e, e, false);
        if !tb.paragraphs.is_empty() {
            let mut tb = tb;
            if tb.body.anchor.is_none() {
                tb.body.anchor = Some(Anchor::Middle);
            }
            for para in &mut tb.paragraphs {
                if para.props.align.is_none() {
                    para.props.align = Some(Align::Center);
                }
            }
            s.text = Some(tb);
        }
        Some(s)
    }

    fn line_shape(&mut self, e: &El) -> Option<Shape> {
        let mut s = self.base_shape(e)?;
        let (x1, y1, x2, y2) = (attr_len(e, "svg:x1")?, attr_len(e, "svg:y1")?, attr_len(e, "svg:x2")?, attr_len(e, "svg:y2")?);
        s.xfrm = Some(Xfrm { x: x1.min(x2), y: y1.min(y2), w: (x2 - x1).abs(), h: (y2 - y1).abs(), rot: 0.0, flip_h: x2 < x1, flip_v: y2 < y1 });
        s.geom = Geom::preset("line");
        s.fill = Some(Fill::None);
        let style = e.attr("draw:style-name");
        s.line = Some(self.line(style).unwrap_or_else(|| Line::solid(ColorRef::rgb(Rgba::rgb(0x34, 0x65, 0xa4)), 0.75)));
        if e.local() == "connector" {
            s.kind = ShapeKind::Connector { start: None, end: None };
        }
        Some(s)
    }

    fn poly(&mut self, e: &El) -> Option<Shape> {
        let mut s = self.base_shape(e)?;
        let vb: Vec<f64> =
            e.attr("svg:viewBox").unwrap_or("").split(|c: char| c.is_whitespace() || c == ',').filter_map(|v| v.parse().ok()).collect();
        let x = s.xfrm.unwrap_or_default();
        let (ox, oy, w, h) = match vb.as_slice() {
            [a, b, c, d] if *c > 0.0 && *d > 0.0 => (*a, *b, *c, *d),
            _ => (0.0, 0.0, x.w.max(1.0), x.h.max(1.0)),
        };
        let closed = e.local() == "polygon";
        let d = if e.local() == "path" {
            svg_path_to_model(e.attr("svg:d").unwrap_or(""), ox, oy)?
        } else {
            let pts: Vec<f64> =
                e.attr("draw:points").unwrap_or("").split(|c: char| c.is_whitespace() || c == ',').filter_map(|v| v.parse().ok()).collect();
            if pts.len() < 4 {
                return None;
            }
            let mut d = String::new();
            for (i, [px, py]) in pts.as_chunks::<2>().0.iter().enumerate() {
                d.push_str(if i == 0 { "M " } else { " L " });
                d.push_str(&format!("{} {}", px - ox, py - oy));
            }
            if closed {
                d.push_str(" Z");
            }
            d
        };
        let open = !d.trim_end().ends_with('Z');
        s.geom = Geom::Custom {
            paths: vec![CustomPath {
                w,
                h,
                d,
                fill: if open { deckcraft_geom::preset::FillMode::None } else { deckcraft_geom::preset::FillMode::Norm },
                stroke: true,
            }],
        };
        let style = e.attr("draw:style-name");
        s.fill = Some(if open {
            Fill::None
        } else {
            self.fill(style, "graphic-properties", false).unwrap_or(Fill::solid(ColorRef::rgb(Rgba::rgb(0x72, 0x9f, 0xcf))))
        });
        s.line = Some(self.line(style).unwrap_or_else(|| Line::solid(ColorRef::rgb(Rgba::rgb(0x34, 0x65, 0xa4)), 0.75)));
        let tb = self.text_body(e, e, false);
        if !tb.paragraphs.is_empty() {
            s.text = Some(tb);
        }
        Some(s)
    }

    fn table(&mut self, t: &El, w: f64, h: f64) -> Table {
        let st = self.st;
        let mut cols = vec![];
        for c in t.children_named("table-column") {
            let n = c.u32("table:number-columns-repeated").unwrap_or(1).clamp(1, 1024);
            let cw = st.prop(c.attr("table:style-name"), "table-column-properties", "style:column-width", false).and_then(len_pt);
            for _ in 0..n {
                cols.push(cw);
            }
        }
        let rows_el: Vec<&El> = {
            let mut v = vec![];
            for e in t.elements() {
                match e.local() {
                    "table-row" => v.push(e),
                    "table-header-rows" | "table-rows" => v.extend(e.children_named("table-row")),
                    _ => {}
                }
            }
            v
        };
        let ncols = rows_el
            .iter()
            .map(|r| {
                r.elements()
                    .filter(|c| c.is("table-cell") || c.is("covered-table-cell"))
                    .map(|c| c.u32("table:number-columns-repeated").unwrap_or(1).clamp(1, 1024) as usize)
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0)
            .max(cols.len())
            .max(1);
        cols.resize(ncols, None);
        let known: f64 = cols.iter().flatten().sum();
        let unknown = cols.iter().filter(|c| c.is_none()).count();
        let each = if unknown > 0 { ((w - known) / unknown as f64).max(18.0) } else { 0.0 };
        let cols: Vec<f64> = cols.into_iter().map(|c| c.unwrap_or(each)).collect();
        let default_h = if rows_el.is_empty() { 0.0 } else { h / rows_el.len() as f64 };
        let mut rows = vec![];
        let mut cells_read = 0usize;
        for r in rows_el {
            let rs = r.attr("table:style-name");
            let height = st
                .prop(rs, "table-row-properties", "style:row-height", false)
                .or_else(|| st.prop(rs, "table-row-properties", "style:min-row-height", false))
                .and_then(len_pt)
                .unwrap_or(default_h);
            let mut cells = vec![];
            for c in r.elements().filter(|c| c.is("table-cell") || c.is("covered-table-cell")) {
                let n = c.u32("table:number-columns-repeated").unwrap_or(1).clamp(1, 1024);
                for _ in 0..n {
                    cells_read += 1;
                    if cells_read > MAX_CELLS || cells.len() >= ncols {
                        break;
                    }
                    cells.push(self.cell(c));
                }
            }
            while cells.len() < ncols {
                cells.push(Cell::new(""));
            }
            rows.push(Row { height, cells });
        }
        // Covered cells: inside a span to the left, or below a vertical span.
        for ri in 0..rows.len() {
            for ci in 0..ncols {
                let (gs, rsn) = rows.get(ri).and_then(|r| r.cells.get(ci)).map(|c| (c.grid_span, c.row_span)).unwrap_or((1, 1));
                for dr in 0..rsn as usize {
                    for dc in 0..gs as usize {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        if let Some(cell) = rows.get_mut(ri + dr).and_then(|r| r.cells.get_mut(ci + dc)) {
                            cell.h_merge = dc > 0;
                            cell.v_merge = dr > 0 && dc == 0;
                        }
                    }
                }
            }
        }
        Table { cols, rows, style: String::new(), first_row: false, band_row: false, ..Default::default() }
    }

    fn cell(&mut self, c: &El) -> Cell {
        let style = c.attr("table:style-name");
        let mut cell = Cell::new("");
        cell.text = self.text_body(c, c, false);
        cell.text.body = BodyProps::default();
        cell.grid_span = c.u32("table:number-columns-spanned").unwrap_or(1).clamp(1, 1024);
        cell.row_span = c.u32("table:number-rows-spanned").unwrap_or(1).clamp(1, 1024);
        if c.is("covered-table-cell") {
            cell.h_merge = true;
        }
        cell.fill = self.fill(style, "graphic-properties", false).or_else(|| {
            self.st
                .prop(style, "table-cell-properties", "fo:background-color", false)
                .and_then(|v| if v == "transparent" { Some(Fill::None) } else { color(v).map(Fill::solid) })
        });
        let st = self.st;
        let pad = |side: &str| st.prop(style, "graphic-properties", &format!("fo:padding-{side}"), false).and_then(len_pt);
        if let (Some(l), Some(r), Some(t), Some(b)) = (pad("left"), pad("right"), pad("top"), pad("bottom")) {
            cell.margins = Some([l, r, t, b]);
        }
        cell.anchor = st.prop(style, "graphic-properties", "draw:textarea-vertical-align", false).and_then(anchor);
        let border = |side: &str| {
            st.prop(style, "table-cell-properties", &format!("fo:border-{side}"), false)
                .or_else(|| st.prop(style, "table-cell-properties", "fo:border", false))
                .or_else(|| st.prop(style, "paragraph-properties", &format!("fo:border-{side}"), false))
                .or_else(|| st.prop(style, "paragraph-properties", "fo:border", false))
                .and_then(border_line)
        };
        cell.borders = [border("left"), border("right"), border("top"), border("bottom")];
        cell
    }

    // --------------------------------------------------------------------------------------- text

    /// The paragraphs in `container` (a text box, a shape, a cell). `shape` carries the frame's
    /// styles: its presentation or graphic style gives the box insets, anchoring and the text
    /// defaults. `bullets`: list items outside a placeholder get a bullet character of their own.
    fn text_body(&mut self, container: &El, shape: &El, bullets: bool) -> TextBody {
        let st = self.st;
        let style = shape.attr("presentation:style-name").or_else(|| shape.attr("draw:style-name"));
        let mut tb = TextBody::default();
        let inset = |side: &str| st.prop(style, "graphic-properties", &format!("fo:padding-{side}"), false).and_then(len_pt);
        tb.body.inset_l = inset("left");
        tb.body.inset_r = inset("right");
        tb.body.inset_t = inset("top");
        tb.body.inset_b = inset("bottom");
        tb.body.anchor = st.prop(style, "graphic-properties", "draw:textarea-vertical-align", false).and_then(anchor);
        // The frame's own text defaults: its style's text properties, then draw:text-style-name.
        let mut base = self.run_props(style);
        base.inherit(&self.run_props(shape.attr("draw:text-style-name")));
        let mut para_base = self.para_props(style);
        para_base.inherit(&self.para_props(shape.attr("draw:text-style-name")));
        let mut ctx = TextCtx { base, para_base, bullets, out: vec![] };
        self.block(container, &mut ctx, None, 0);
        tb.paragraphs = ctx.out;
        tb
    }

    fn block(&mut self, e: &El, ctx: &mut TextCtx, list: Option<(u8, bool)>, depth: usize) {
        if depth > MAX_NEST {
            return;
        }
        for c in e.elements() {
            match c.local() {
                "p" | "h" => {
                    let mut para = Paragraph::default();
                    let pstyle = c.attr("text:style-name");
                    let mut props = self.para_props(pstyle);
                    props.inherit(&ctx.para_base);
                    let mut rp = self.run_props(pstyle);
                    rp.inherit(&ctx.base);
                    if let Some((level, item)) = list {
                        para.level = level;
                        if !item {
                            props.bullet = Some(Bullet::None);
                        } else if ctx.bullets && props.bullet.is_none() {
                            props.bullet = Some(Bullet::Char { char: "•".into() });
                        }
                    }
                    para.props = props;
                    let mut runs = vec![];
                    self.inline(c, &rp, &mut runs, 0);
                    trim_runs(&mut runs);
                    para.runs = runs;
                    para.end_props = rp;
                    para.normalize();
                    ctx.out.push(para);
                }
                "list" => {
                    let level = list.map(|(l, _)| l.saturating_add(1).min(8)).unwrap_or(0);
                    for item in c.elements() {
                        let is_item = item.is("list-item");
                        if is_item || item.is("list-header") {
                            self.block(item, ctx, Some((level, is_item)), depth + 1);
                        }
                    }
                }
                "section" | "text-box" => self.block(c, ctx, list, depth + 1),
                _ => {}
            }
        }
    }

    fn inline(&mut self, e: &El, props: &RunProps, out: &mut Vec<Run>, depth: usize) {
        if depth > MAX_NEST {
            return;
        }
        for n in &e.children {
            match n {
                Node::Text(t) => out.push(Run::with(collapse_ws(t), props.clone())),
                Node::El(c) => match c.local() {
                    "span" | "a" | "meta" | "ruby-base" => {
                        let mut p = self.run_props(c.attr("text:style-name"));
                        p.inherit(props);
                        if c.is("a")
                            && let Some(h) = c.attr("xlink:href").filter(|h| h.contains(':'))
                        {
                            p.link = Some(deckcraft_model::text::Hyperlink {
                                action: deckcraft_model::text::Action::Url { url: h.to_string() },
                                tooltip: String::new(),
                                highlight_click: false,
                                sound: None,
                                stop_sound: false,
                                zoom_return: None,
                            });
                        }
                        self.inline(c, &p, out, depth + 1);
                    }
                    "s" => out.push(Run::with(" ".repeat(c.u32("text:c").unwrap_or(1).clamp(1, 1000) as usize), props.clone())),
                    "tab" => out.push(Run::with("\t", props.clone())),
                    "line-break" => out.push(Run { text: String::new(), props: props.clone(), kind: RunKind::Break }),
                    "page-number" => out.push(Run { text: c.text(), props: props.clone(), kind: RunKind::Field { field: "slidenum".into() } }),
                    "date" | "time" => out.push(Run { text: c.text(), props: props.clone(), kind: RunKind::Field { field: "datetime".into() } }),
                    "note" | "annotation" | "bookmark" | "bookmark-start" | "bookmark-end" | "soft-page-break" | "ruby-text" => {}
                    _ => self.inline(c, props, out, depth + 1),
                },
            }
        }
    }

    fn run_props(&self, style: Option<&str>) -> RunProps {
        let st = self.st;
        let get = |a: &str| st.prop(style, "text-properties", a, false);
        let mut r = RunProps::default();
        if style.is_none() {
            return r;
        }
        r.size = get("fo:font-size").and_then(len_pt).filter(|v| *v > 0.0 && *v < 4000.0);
        r.bold = get("fo:font-weight").map(|w| w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600));
        r.italic = get("fo:font-style").map(|s| s == "italic" || s == "oblique");
        r.font = get("style:font-name")
            .map(|n| st.fonts.get(n).cloned().unwrap_or_else(|| n.to_string()))
            .or_else(|| get("fo:font-family").map(|f| f.trim_matches(|c| c == '\'' || c == '"').to_string()));
        r.font_ea = get("style:font-name-asian").map(|n| st.fonts.get(n).cloned().unwrap_or_else(|| n.to_string()));
        r.font_cs = get("style:font-name-complex").map(|n| st.fonts.get(n).cloned().unwrap_or_else(|| n.to_string()));
        if get("style:use-window-font-color") != Some("true") {
            r.fill = get("fo:color").and_then(color).map(Fill::solid);
        }
        r.underline = get("style:text-underline-style").map(|u| if u == "none" { "none".into() } else { "sng".into() });
        r.strike = get("style:text-line-through-style").map(|u| if u == "none" { Strike::None } else { Strike::Single });
        r.baseline = get("style:text-position").and_then(|p| match p.split_whitespace().next()? {
            "super" => Some(0.33),
            "sub" => Some(-0.33),
            v => v.trim_end_matches('%').parse::<f64>().ok().map(|x| (x / 100.0).clamp(-1.0, 1.0)).filter(|x| *x != 0.0),
        });
        r.lang = get("fo:language").filter(|l| *l != "zxx").map(|l| match get("fo:country") {
            Some(c) if c != "none" => format!("{l}-{c}"),
            _ => l.to_string(),
        });
        r
    }

    fn para_props(&self, style: Option<&str>) -> ParaProps {
        let st = self.st;
        let mut p = ParaProps::default();
        if style.is_none() {
            return p;
        }
        let get = |a: &str| st.prop(style, "paragraph-properties", a, false);
        p.align = get("fo:text-align").and_then(|a| match a {
            "center" => Some(Align::Center),
            "end" | "right" => Some(Align::Right),
            "justify" => Some(Align::Justify),
            "start" | "left" => Some(Align::Left),
            _ => None,
        });
        p.margin_left = get("fo:margin-left").and_then(len_pt);
        p.margin_right = get("fo:margin-right").and_then(len_pt);
        p.indent = get("fo:text-indent").and_then(len_pt);
        p.space_before = get("fo:margin-top").and_then(len_pt).map(Spacing::Pts);
        p.space_after = get("fo:margin-bottom").and_then(len_pt).map(Spacing::Pts);
        p.line_spacing = get("fo:line-height").and_then(|v| {
            if let Some(pc) = v.strip_suffix('%') {
                pc.trim().parse::<f64>().ok().map(|x| Spacing::Pct((x / 100.0).clamp(0.0, 100.0)))
            } else {
                len_pt(v).map(Spacing::Pts)
            }
        });
        p.rtl = get("style:writing-mode").map(|m| m.starts_with("rl"));
        p
    }
}

struct TextCtx {
    base: RunProps,
    para_base: ParaProps,
    bullets: bool,
    out: Vec<Paragraph>,
}

/// ODF collapses runs of white space in text content to one space (`text:s` writes the others).
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

/// A paragraph's leading and trailing collapsed white space is not text.
fn trim_runs(runs: &mut Vec<Run>) {
    if let Some(r) = runs.iter_mut().find(|r| r.kind == RunKind::Text && !r.text.is_empty()) {
        r.text = r.text.trim_start_matches(' ').to_string();
    }
    if let Some(r) = runs.iter_mut().rev().find(|r| r.kind == RunKind::Text && !r.text.is_empty()) {
        r.text = r.text.trim_end_matches(' ').to_string();
    }
    runs.retain(|r| r.kind != RunKind::Text || !r.text.is_empty());
}

fn anchor(v: &str) -> Option<Anchor> {
    match v {
        "top" => Some(Anchor::Top),
        "middle" => Some(Anchor::Middle),
        "bottom" => Some(Anchor::Bottom),
        "justify" => Some(Anchor::Justified),
        _ => None,
    }
}

/// `fo:border` (`0.03cm solid #000000`, `none`).
fn border_line(v: &str) -> Option<Line> {
    if v.trim() == "none" || v.trim().is_empty() {
        return Some(Line::none());
    }
    let mut w = None;
    let mut c = None;
    let mut dash = None;
    for t in v.split_whitespace() {
        if let Some(x) = len_pt(t) {
            w = Some(x);
        } else if t.starts_with('#') {
            c = color(t);
        } else if matches!(t, "dashed" | "dotted") {
            dash = Some(Dash::Preset { name: if t == "dotted" { "sysDot".into() } else { "dash".into() } });
        }
    }
    let mut l = Line::solid(c.unwrap_or(ColorRef::rgb(Rgba::BLACK)), w.unwrap_or(0.75));
    l.dash = dash;
    Some(l)
}

fn gradient(g: &El) -> Fill {
    let start = g.attr("draw:start-color").and_then(color).unwrap_or(ColorRef::rgb(Rgba::BLACK));
    let end = g.attr("draw:end-color").and_then(color).unwrap_or(ColorRef::rgb(Rgba::WHITE));
    let style = g.attr("draw:style").unwrap_or("linear");
    // draw:angle: tenths of a degree (or with a unit), counter-clockwise; 0 runs top to bottom.
    let a = g.attr("draw:angle").map(|v| {
        let v = v.trim();
        if let Some(d) = v.strip_suffix("deg") {
            d.parse::<f64>().unwrap_or(0.0)
        } else if let Some(r) = v.strip_suffix("rad") {
            r.parse::<f64>().unwrap_or(0.0).to_degrees()
        } else {
            v.parse::<f64>().unwrap_or(0.0) / 10.0
        }
    });
    let angle = (90.0 - a.unwrap_or(0.0)).rem_euclid(360.0);
    let (stops, shape) = match style {
        "axial" => (
            vec![GradientStop { pos: 0.0, color: end.clone() }, GradientStop { pos: 0.5, color: start }, GradientStop { pos: 1.0, color: end }],
            GradientShape::Linear { angle, scaled: false },
        ),
        "radial" | "ellipsoid" | "square" | "rectangular" => (
            vec![GradientStop { pos: 0.0, color: end }, GradientStop { pos: 1.0, color: start }],
            GradientShape::Path {
                path: if matches!(style, "square" | "rectangular") { "rect".into() } else { "circle".into() },
                focus: [0.5, 0.5, 0.5, 0.5],
            },
        ),
        _ => (vec![GradientStop { pos: 0.0, color: start }, GradientStop { pos: 1.0, color: end }], GradientShape::Linear { angle, scaled: false }),
    };
    Fill::Gradient(Gradient { stops, shape, rotate_with_shape: true, tile: [0.0; 4] })
}

/// Page names LibreOffice generates (`page1`, `Slide 3`) are not names a user gave.
fn is_generated_page_name(n: &str) -> bool {
    let t = n.trim();
    ["page", "Slide ", "Slide"].iter().any(|p| t.strip_prefix(p).is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())))
}

/// Picture formats the renderer draws (StarView metafiles, `.svm`, are LibreOffice-only previews).
fn drawable_picture(part: &str) -> bool {
    let ext = part.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "jpe" | "gif" | "bmp" | "tif" | "tiff" | "svg" | "emf" | "wmf" | "webp")
}

/// A shape's box: `svg:x/y/width/height`, with `draw:transform` (`rotate (r) translate (x y)`,
/// applied in order, ODF's rotation counter-clockwise in radians) giving the position and turn.
fn xfrm(e: &El) -> Xfrm {
    let w = attr_len(e, "svg:width").unwrap_or(0.0).max(0.0);
    let h = attr_len(e, "svg:height").unwrap_or(0.0).max(0.0);
    let x0 = attr_len(e, "svg:x").unwrap_or(0.0);
    let y0 = attr_len(e, "svg:y").unwrap_or(0.0);
    let Some(t) = e.attr("draw:transform") else {
        return Xfrm { x: x0, y: y0, w, h, ..Default::default() };
    };
    // Track where the box's centre goes and the total turn.
    let (mut cx, mut cy) = (x0 + w / 2.0, y0 + h / 2.0);
    let mut turn = 0.0f64;
    let mut rest = t;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let Some(close) = rest[open..].find(')') else { break };
        let args: Vec<f64> =
            rest[open + 1..open + close].split(|c: char| c.is_whitespace() || c == ',').filter(|s| !s.is_empty()).filter_map(len_or_num).collect();
        match (name, args.as_slice()) {
            ("rotate", [r, ..]) => {
                let (s, c) = r.sin_cos();
                (cx, cy) = (cx * c + cy * s, -cx * s + cy * c);
                turn += r;
            }
            ("translate", [tx]) => cx += tx,
            ("translate", [tx, ty, ..]) => {
                cx += tx;
                cy += ty;
            }
            _ => {}
        }
        rest = &rest[open + close + 1..];
    }
    let rot = (-turn.to_degrees()).rem_euclid(360.0);
    let rot = if rot.is_finite() && (rot - 360.0).abs() > 1e-6 { rot } else { 0.0 };
    Xfrm { x: cx - w / 2.0, y: cy - h / 2.0, w, h, rot, ..Default::default() }
}

/// A transform argument: a length with a unit, or a plain number (radians, or 1/100 mm lengths are
/// never written without a unit in practice).
fn len_or_num(s: &str) -> Option<f64> {
    if s.ends_with(|c: char| c.is_ascii_alphabetic()) { len_pt(s) } else { s.parse().ok() }
}

/// Custom-shape geometry: the OOXML preset an ODF shape type is, `ooxml-<preset>` as itself, else
/// its own enhanced path when that is plain coordinates, else a rectangle.
fn geometry(g: &El) -> Geom {
    let ty = g.attr("draw:type").unwrap_or("");
    if let Some(name) = ty.strip_prefix("ooxml-")
        && deckcraft_geom::preset::info(name).is_some()
    {
        return Geom::preset(name);
    }
    if let Some(name) = preset_for(ty)
        && deckcraft_geom::preset::info(name).is_some()
    {
        return Geom::preset(name);
    }
    enhanced_path(g).unwrap_or_default()
}

fn preset_for(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "rectangle" | "mso-spt1" => "rect",
        "round-rectangle" | "mso-spt2" => "roundRect",
        "ellipse" | "circle" => "ellipse",
        "isosceles-triangle" => "triangle",
        "right-triangle" => "rtTriangle",
        "trapezoid" => "trapezoid",
        "diamond" => "diamond",
        "parallelogram" => "parallelogram",
        "pentagon" => "pentagon",
        "hexagon" => "hexagon",
        "octagon" => "octagon",
        "cross" => "plus",
        "star4" => "star4",
        "star5" => "star5",
        "star6" => "star6",
        "star8" => "star8",
        "star12" => "star12",
        "star24" => "star24",
        "concave-star6" => "star6",
        "can" => "can",
        "cube" => "cube",
        "ring" => "donut",
        "block-arc" => "blockArc",
        "heart" => "heart",
        "moon" => "moon",
        "smiley" => "smileyFace",
        "sun" => "sun",
        "forbidden" => "noSmoking",
        "frame" => "frame",
        "paper" => "foldedCorner",
        "quad-bevel" => "bevel",
        "bracket-pair" => "bracketPair",
        "brace-pair" => "bracePair",
        "left-bracket" => "leftBracket",
        "right-bracket" => "rightBracket",
        "left-brace" => "leftBrace",
        "right-brace" => "rightBrace",
        "horizontal-scroll" => "horizontalScroll",
        "vertical-scroll" => "verticalScroll",
        "bang" => "irregularSeal1",
        "flower" => "flowChartPunchedTape",
        "rectangular-callout" => "wedgeRectCallout",
        "round-rectangular-callout" => "wedgeRoundRectCallout",
        "round-callout" => "wedgeEllipseCallout",
        "cloud-callout" | "cloud" => "cloudCallout",
        "line-callout-1" => "borderCallout1",
        "line-callout-2" => "borderCallout2",
        "line-callout-3" => "borderCallout3",
        "circular-arrow" => "circularArrow",
        "right-arrow" => "rightArrow",
        "left-arrow" => "leftArrow",
        "up-arrow" => "upArrow",
        "down-arrow" => "downArrow",
        "left-right-arrow" => "leftRightArrow",
        "up-down-arrow" => "upDownArrow",
        "quad-arrow" => "quadArrow",
        "notched-right-arrow" => "notchedRightArrow",
        "striped-right-arrow" => "stripedRightArrow",
        "chevron" => "chevron",
        "pentagon-right" => "homePlate",
        "right-arrow-callout" => "rightArrowCallout",
        "left-arrow-callout" => "leftArrowCallout",
        "up-arrow-callout" => "upArrowCallout",
        "down-arrow-callout" => "downArrowCallout",
        "flowchart-process" => "flowChartProcess",
        "flowchart-alternate-process" => "flowChartAlternateProcess",
        "flowchart-decision" => "flowChartDecision",
        "flowchart-data" => "flowChartInputOutput",
        "flowchart-predefined-process" => "flowChartPredefinedProcess",
        "flowchart-internal-storage" => "flowChartInternalStorage",
        "flowchart-document" => "flowChartDocument",
        "flowchart-multidocument" => "flowChartMultidocument",
        "flowchart-terminator" => "flowChartTerminator",
        "flowchart-preparation" => "flowChartPreparation",
        "flowchart-manual-input" => "flowChartManualInput",
        "flowchart-manual-operation" => "flowChartManualOperation",
        "flowchart-connector" => "flowChartConnector",
        "flowchart-off-page-connector" => "flowChartOffpageConnector",
        "flowchart-card" => "flowChartPunchedCard",
        "flowchart-punched-tape" => "flowChartPunchedTape",
        "flowchart-summing-junction" => "flowChartSummingJunction",
        "flowchart-or" => "flowChartOr",
        "flowchart-collate" => "flowChartCollate",
        "flowchart-sort" => "flowChartSort",
        "flowchart-extract" => "flowChartExtract",
        "flowchart-merge" => "flowChartMerge",
        "flowchart-stored-data" => "flowChartOnlineStorage",
        "flowchart-delay" => "flowChartDelay",
        "flowchart-sequential-access" => "flowChartMagneticTape",
        "flowchart-magnetic-disk" => "flowChartMagneticDisk",
        "flowchart-direct-access-storage" => "flowChartMagneticDrum",
        "flowchart-display" => "flowChartDisplay",
        _ => return None,
    })
}

/// `draw:enhanced-path` as a model path, when it is only numbers and the commands M L C Q Z N F S
/// (formula references and arcs are refused, and the caller draws a rectangle).
fn enhanced_path(g: &El) -> Option<Geom> {
    let d = g.attr("draw:enhanced-path")?;
    let vb: Vec<f64> = g.attr("svg:viewBox").unwrap_or("0 0 21600 21600").split_whitespace().filter_map(|v| v.parse().ok()).collect();
    let [ox, oy, w, h] = vb.as_slice() else { return None };
    if *w <= 0.0 || *h <= 0.0 {
        return None;
    }
    let mut paths = vec![];
    let mut cur = String::new();
    let mut fill = true;
    let mut stroke = true;
    let mut cmd = 'M';
    let mut nums: Vec<f64> = vec![];
    let flush_cmd = |cmd: char, nums: &mut Vec<f64>, cur: &mut String| -> Option<()> {
        let per = match cmd {
            'M' | 'L' => 2,
            'C' => 6,
            'Q' => 4,
            _ => return if nums.is_empty() { Some(()) } else { None },
        };
        if !nums.len().is_multiple_of(per) {
            return None;
        }
        for (i, chunk) in nums.chunks_exact(per).enumerate() {
            let c = if cmd == 'M' && i > 0 { 'L' } else { cmd };
            cur.push_str(&format!(" {c}"));
            for (k, v) in chunk.iter().enumerate() {
                cur.push_str(&format!(" {}", v - if k % 2 == 0 { ox } else { oy }));
            }
        }
        nums.clear();
        Some(())
    };
    for t in d.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()) {
        if let Ok(v) = t.parse::<f64>() {
            nums.push(v);
            continue;
        }
        let mut chars = t.chars();
        let c = chars.next()?;
        let tail: String = chars.collect();
        flush_cmd(cmd, &mut nums, &mut cur)?;
        match c {
            'M' | 'L' | 'C' | 'Q' => cmd = c,
            'Z' => {
                cur.push_str(" Z");
                cmd = 'Z';
            }
            'N' => {
                if !cur.trim().is_empty() {
                    paths.push(CustomPath {
                        w: *w,
                        h: *h,
                        d: cur.trim().to_string(),
                        fill: if fill { deckcraft_geom::preset::FillMode::Norm } else { deckcraft_geom::preset::FillMode::None },
                        stroke,
                    });
                }
                cur.clear();
                fill = true;
                stroke = true;
                cmd = 'M';
            }
            'F' => fill = false,
            'S' => stroke = false,
            _ => return None,
        }
        if !tail.is_empty() {
            nums.push(tail.parse().ok()?);
        }
    }
    flush_cmd(cmd, &mut nums, &mut cur)?;
    if !cur.trim().is_empty() {
        paths.push(CustomPath {
            w: *w,
            h: *h,
            d: cur.trim().to_string(),
            fill: if fill { deckcraft_geom::preset::FillMode::Norm } else { deckcraft_geom::preset::FillMode::None },
            stroke,
        });
    }
    (!paths.is_empty()).then_some(Geom::Custom { paths })
}

/// An SVG path (`svg:d`) as the model's absolute `M L C Q Z` commands, shifted by the view box
/// origin. Relative commands, `H V S T` are resolved; an arc becomes a line to its end point.
fn svg_path_to_model(d: &str, ox: f64, oy: f64) -> Option<String> {
    let mut toks: Vec<String> = vec![];
    let mut num = String::new();
    let flush = |num: &mut String, toks: &mut Vec<String>| {
        if !num.is_empty() {
            toks.push(std::mem::take(num));
        }
    };
    for c in d.chars() {
        if c.is_ascii_alphabetic() && c != 'e' && c != 'E' {
            flush(&mut num, &mut toks);
            toks.push(c.to_string());
        } else if c == ',' || c.is_whitespace() {
            flush(&mut num, &mut toks);
        } else if (c == '-' && !num.is_empty() && !num.ends_with(['e', 'E'])) || (c == '.' && num.contains('.') && !num.contains(['e', 'E'])) {
            // A sign or a second point starts the next number.
            flush(&mut num, &mut toks);
            num.push(c);
        } else {
            num.push(c);
        }
    }
    flush(&mut num, &mut toks);
    let mut out = String::new();
    let (mut x, mut y, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut last_ctrl: Option<(f64, f64)> = None;
    let mut cmd = 'M';
    let mut i = 0usize;
    let num_at = |i: usize| -> Option<f64> { toks.get(i).and_then(|t| t.parse::<f64>().ok()) };
    let mut guard = 0usize;
    while i < toks.len() {
        guard += 1;
        if guard > 1_000_000 {
            return None;
        }
        if let Some(t) = toks.get(i)
            && t.parse::<f64>().is_err()
        {
            cmd = t.chars().next()?;
            i += 1;
            if matches!(cmd, 'Z' | 'z') {
                out.push_str(" Z");
                x = sx;
                y = sy;
                last_ctrl = None;
                continue;
            }
        }
        let rel = cmd.is_ascii_lowercase();
        let (bx, by) = if rel { (x, y) } else { (0.0, 0.0) };
        let p = |k: usize| num_at(i + k);
        match cmd.to_ascii_uppercase() {
            'M' | 'L' => {
                let (nx, ny) = (bx + p(0)?, by + p(1)?);
                out.push_str(&format!(" {} {} {}", if cmd.eq_ignore_ascii_case(&'M') { 'M' } else { 'L' }, nx - ox, ny - oy));
                if cmd.eq_ignore_ascii_case(&'M') {
                    sx = nx;
                    sy = ny;
                    cmd = if rel { 'l' } else { 'L' };
                }
                (x, y) = (nx, ny);
                last_ctrl = None;
                i += 2;
            }
            'H' => {
                x = bx + p(0)?;
                out.push_str(&format!(" L {} {}", x - ox, y - oy));
                last_ctrl = None;
                i += 1;
            }
            'V' => {
                y = (if rel { y } else { 0.0 }) + p(0)?;
                out.push_str(&format!(" L {} {}", x - ox, y - oy));
                last_ctrl = None;
                i += 1;
            }
            'C' => {
                let (x1, y1, x2, y2, nx, ny) = (bx + p(0)?, by + p(1)?, bx + p(2)?, by + p(3)?, bx + p(4)?, by + p(5)?);
                out.push_str(&format!(" C {} {} {} {} {} {}", x1 - ox, y1 - oy, x2 - ox, y2 - oy, nx - ox, ny - oy));
                last_ctrl = Some((x2, y2));
                (x, y) = (nx, ny);
                i += 6;
            }
            'S' => {
                let (x1, y1) = last_ctrl.map(|(cx, cy)| (2.0 * x - cx, 2.0 * y - cy)).unwrap_or((x, y));
                let (x2, y2, nx, ny) = (bx + p(0)?, by + p(1)?, bx + p(2)?, by + p(3)?);
                out.push_str(&format!(" C {} {} {} {} {} {}", x1 - ox, y1 - oy, x2 - ox, y2 - oy, nx - ox, ny - oy));
                last_ctrl = Some((x2, y2));
                (x, y) = (nx, ny);
                i += 4;
            }
            'Q' => {
                let (x1, y1, nx, ny) = (bx + p(0)?, by + p(1)?, bx + p(2)?, by + p(3)?);
                out.push_str(&format!(" Q {} {} {} {}", x1 - ox, y1 - oy, nx - ox, ny - oy));
                last_ctrl = Some((x1, y1));
                (x, y) = (nx, ny);
                i += 4;
            }
            'T' => {
                let (x1, y1) = last_ctrl.map(|(cx, cy)| (2.0 * x - cx, 2.0 * y - cy)).unwrap_or((x, y));
                let (nx, ny) = (bx + p(0)?, by + p(1)?);
                out.push_str(&format!(" Q {} {} {} {}", x1 - ox, y1 - oy, nx - ox, ny - oy));
                last_ctrl = Some((x1, y1));
                (x, y) = (nx, ny);
                i += 2;
            }
            'A' => {
                let (nx, ny) = (bx + p(5)?, by + p(6)?);
                out.push_str(&format!(" L {} {}", nx - ox, ny - oy));
                (x, y) = (nx, ny);
                last_ctrl = None;
                i += 7;
            }
            _ => return None,
        }
    }
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
#[path = "odp_tests.rs"]
mod tests;
