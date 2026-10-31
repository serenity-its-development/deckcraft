//! OpenDocument Presentation (`.odp`) export, written from OASIS ODF 1.3.
//!
//! Every slide is a `draw:page`; what DeckCraft inherits (placeholder boxes, theme colours, master
//! text styles, style-matrix fills and lines) is resolved and written out explicitly, so the file
//! draws the same without DeckCraft's masters. Written: slide size, pages (name, hidden), solid,
//! gradient and picture backgrounds, title/subtitle/outline/notes placeholders, text boxes,
//! preset and custom shapes (`draw:type="ooxml-<preset>"` with the outline as the enhanced
//! path, so any reader draws it), lines and connectors, pictures, tables (widths, heights, spans,
//! fills, borders), groups, rotation, fills (solid, gradient, picture), lines (colour, width,
//! dash), text (paragraph alignment, indents, spacing, list levels and bullets, runs with font,
//! size, bold, italic, underline, strike-through, colour, baseline, links, line breaks, slide
//! number fields) and speaker notes. Not written: animations, transitions, charts, media, ink,
//! 3-D models and SmartArt (their fallback picture is written when they have one), comments.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;

use deckcraft_color::Rgba;
use deckcraft_geom::{Affine, PathEl, Point, Xfrm};
use deckcraft_model::resolve::{self, Ctx};
use deckcraft_model::style::{Dash, Fill, GradientShape, Line, PictureMode};
use deckcraft_model::table::Table;
use deckcraft_model::text::{Align, Anchor, Bullet, RunKind, RunProps, Spacing, Strike, TextBody};
use deckcraft_model::{Geom, MediaId, PhType, Presentation, Shape, ShapeKind, Slide};

use crate::PptxError;
use crate::xml::{esc, esc_attr};

const MIME: &str = "application/vnd.oasis.opendocument.presentation";

const NS: &str = concat!(
    r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
    r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
    r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
    r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
    r#"xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" "#,
    r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
    r#"xmlns:xlink="http://www.w3.org/1999/xlink" "#,
    r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
    r#"xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" "#,
    r#"xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" "#,
    r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" "#,
    r#"office:version="1.3""#
);

/// Nesting of groups followed.
const MAX_DEPTH: usize = 64;

/// Encode `p` as an `.odp` package.
pub fn export(p: &Presentation) -> Result<Vec<u8>, PptxError> {
    let mut w = Writer::new(p);
    let mut body = String::new();
    for (i, slide) in p.slides.iter().enumerate() {
        w.page(i, slide, &mut body);
    }
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:automatic-styles>{}</office:automatic-styles><office:body><office:presentation>{body}</office:presentation></office:body></office:document-content>"#,
        w.auto
    );
    let size = p.slide_size;
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:styles>{}</office:styles><office:automatic-styles><style:page-layout style:name="PM1"><style:page-layout-properties fo:margin-top="0pt" fo:margin-bottom="0pt" fo:margin-left="0pt" fo:margin-right="0pt" fo:page-width="{}" fo:page-height="{}" style:print-orientation="{}"/></style:page-layout><style:style style:name="Mdp1" style:family="drawing-page"><style:drawing-page-properties draw:fill="none"/></style:style></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="PM1" draw:style-name="Mdp1"/></office:master-styles></office:document-styles>"#,
        w.common,
        pt(size.width),
        pt(size.height),
        if size.width >= size.height { "landscape" } else { "portrait" }
    );
    let pr = &p.props;
    let mut meta = String::from("<meta:generator>DeckCraft</meta:generator>");
    for (tag, v) in [("dc:title", &pr.title), ("dc:subject", &pr.subject), ("meta:initial-creator", &pr.author), ("dc:description", &pr.comments)] {
        if !v.is_empty() {
            let _ = write!(meta, "<{tag}>{}</{tag}>", esc(v));
        }
    }
    let meta =
        format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-meta {NS}><office:meta>{meta}</office:meta></office:document-meta>"#);
    let mut manifest = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">"#,
        r#"<manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.presentation"/>"#,
        r#"<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>"#,
        r#"<manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>"#,
        r#"<manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>"#
    ));
    let mut pictures: Vec<(&String, &MediaId)> = w.pictures.iter().collect();
    pictures.sort_by_key(|(name, _)| name.as_str());
    for (name, id) in &pictures {
        let ct = p.media(**id).map(|m| m.content_type.clone()).unwrap_or_default();
        let _ = write!(manifest, r#"<manifest:file-entry manifest:full-path="{}" manifest:media-type="{}"/>"#, esc_attr(name), esc_attr(&ct));
    }
    manifest.push_str("</manifest:manifest>");

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let ze = |e: zip::result::ZipError| PptxError::Write(e.to_string());
        let io = |e: std::io::Error| PptxError::Write(e.to_string());
        // The mimetype first and stored: readers identify the package from its first bytes.
        z.start_file("mimetype", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).map_err(ze)?;
        z.write_all(MIME.as_bytes()).map_err(io)?;
        let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in [("content.xml", content.as_bytes()), ("styles.xml", styles.as_bytes()), ("meta.xml", meta.as_bytes())] {
            z.start_file(name, deflate).map_err(ze)?;
            z.write_all(data).map_err(io)?;
        }
        for (name, id) in pictures {
            let Some(m) = p.media(*id) else { continue };
            z.start_file(name.as_str(), zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).map_err(ze)?;
            z.write_all(&m.data).map_err(io)?;
        }
        z.start_file("META-INF/manifest.xml", deflate).map_err(ze)?;
        z.write_all(manifest.as_bytes()).map_err(io)?;
        z.finish().map_err(ze)?;
    }
    Ok(buf.into_inner())
}

/// A length in points as ODF writes it.
fn pt(v: f64) -> String {
    format!("{}pt", num(v))
}

fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" { "0".into() } else { s.to_string() }
}

fn hex(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

struct Writer<'a> {
    p: &'a Presentation,
    /// `content.xml`'s automatic styles.
    auto: String,
    /// `styles.xml`'s common styles (gradients, fill images).
    common: String,
    /// Each automatic style once, by its properties.
    styles: HashMap<(char, String), String>,
    /// Picture parts written, `Pictures/…` → media.
    pictures: HashMap<String, MediaId>,
    by_media: HashMap<MediaId, String>,
    gradients: HashMap<String, String>,
    fill_images: HashMap<MediaId, String>,
}

impl<'a> Writer<'a> {
    fn new(p: &'a Presentation) -> Self {
        Writer {
            p,
            auto: String::new(),
            common: String::new(),
            styles: HashMap::new(),
            pictures: HashMap::new(),
            by_media: HashMap::new(),
            gradients: HashMap::new(),
            fill_images: HashMap::new(),
        }
    }

    /// An automatic style of `family` whose child elements are `props`, written once.
    fn style(&mut self, kind: char, family: &str, props: &str) -> String {
        let key = (kind, props.to_string());
        if let Some(n) = self.styles.get(&key) {
            return n.clone();
        }
        let name = format!("{kind}{}", self.styles.keys().filter(|(k, _)| *k == kind).count() + 1);
        let _ = write!(self.auto, r#"<style:style style:name="{name}" style:family="{family}">{props}</style:style>"#);
        self.styles.insert(key, name.clone());
        name
    }

    fn picture(&mut self, id: MediaId) -> Option<String> {
        if let Some(n) = self.by_media.get(&id) {
            return Some(n.clone());
        }
        let m = self.p.media(id)?;
        if m.data.is_empty() {
            return None;
        }
        let ext = m
            .name
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .unwrap_or_else(|| "bin".into());
        let name = format!("Pictures/media{}.{ext}", id.0);
        self.pictures.insert(name.clone(), id);
        self.by_media.insert(id, name.clone());
        Some(name)
    }

    /// `draw:fill…` attributes for a resolved fill.
    fn fill_attrs(&mut self, ctx: &Ctx, fill: Option<&Fill>, ph: Option<Rgba>) -> String {
        match fill {
            None | Some(Fill::None | Fill::Group | Fill::Background) => r#" draw:fill="none""#.into(),
            Some(Fill::Solid { color }) => {
                let c = ctx.color(color, ph);
                let mut s = format!(r#" draw:fill="solid" draw:fill-color="{}""#, hex(c));
                let alpha = color.alpha() * f64::from(c.a) / 255.0;
                if alpha < 0.999 {
                    let _ = write!(s, r#" draw:opacity="{}%""#, num(alpha * 100.0));
                }
                s
            }
            Some(Fill::Pattern(pf)) => format!(r#" draw:fill="solid" draw:fill-color="{}""#, hex(ctx.color(&pf.fg, ph))),
            Some(Fill::Gradient(g)) => {
                let (Some(first), Some(last)) = (g.stops.first(), g.stops.last()) else { return r#" draw:fill="none""#.into() };
                let (a, b) = (hex(ctx.color(&first.color, ph)), hex(ctx.color(&last.color, ph)));
                let (style, angle) = match &g.shape {
                    GradientShape::Linear { angle, .. } => ("linear", (90.0 - angle).rem_euclid(360.0)),
                    GradientShape::Path { path, .. } if path == "rect" => ("rectangular", 0.0),
                    GradientShape::Path { .. } => ("radial", 0.0),
                };
                // Radial gradients run from the outside in, as the reader reads them.
                let (start, end) = if style == "linear" { (a, b) } else { (b, a) };
                let key = format!("{style}{start}{end}{angle}");
                let n = self.gradients.len();
                let name = self
                    .gradients
                    .entry(key)
                    .or_insert_with(|| {
                        let name = format!("Gradient_{}", n + 1);
                        let _ = write!(
                            self.common,
                            r#"<draw:gradient draw:name="{name}" draw:style="{style}" draw:start-color="{start}" draw:end-color="{end}" draw:angle="{}deg" draw:border="0%" draw:cx="50%" draw:cy="50%" draw:start-intensity="100%" draw:end-intensity="100%"/>"#,
                            num(angle)
                        );
                        name
                    })
                    .clone();
                format!(r#" draw:fill="gradient" draw:fill-gradient-name="{name}""#)
            }
            Some(Fill::Picture(pf)) => {
                let Some(part) = self.picture(pf.media) else { return r#" draw:fill="none""#.into() };
                let n = self.fill_images.len();
                let name = self
                    .fill_images
                    .entry(pf.media)
                    .or_insert_with(|| {
                        let name = format!("Image_{}", n + 1);
                        let _ = write!(
                            self.common,
                            r#"<draw:fill-image draw:name="{name}" xlink:href="{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#,
                            esc_attr(&part)
                        );
                        name
                    })
                    .clone();
                let repeat = if matches!(pf.mode, PictureMode::Tile { .. }) { "repeat" } else { "stretch" };
                format!(r#" draw:fill="bitmap" draw:fill-image-name="{name}" style:repeat="{repeat}""#)
            }
        }
    }

    fn line_attrs(ctx: &Ctx, line: &Line, ph: Option<Rgba>) -> String {
        let color = match &line.fill {
            None | Some(Fill::None) => None,
            Some(Fill::Solid { color }) => Some(ctx.color(color, ph)),
            Some(Fill::Gradient(g)) => g.stops.first().map(|s| ctx.color(&s.color, ph)),
            Some(Fill::Pattern(pf)) => Some(ctx.color(&pf.fg, ph)),
            Some(_) => ph,
        };
        let Some(c) = color else { return r#" draw:stroke="none""#.into() };
        let dashed = matches!(&line.dash, Some(Dash::Preset { name }) if name != "solid") || matches!(&line.dash, Some(Dash::Custom { .. }));
        format!(
            r#" draw:stroke="{}" svg:stroke-color="{}" svg:stroke-width="{}""#,
            if dashed { "dash" } else { "solid" },
            hex(c),
            pt(line.width.unwrap_or(0.75))
        )
    }

    // ------------------------------------------------------------------------------------ pages

    fn page(&mut self, index: usize, slide: &Slide, out: &mut String) {
        let p = self.p;
        let Some(ctx) = Ctx::for_slide(p, slide) else { return };
        let (bg, ph) = resolve::background(&ctx, Some(slide));
        let fill = self.fill_attrs(&ctx, Some(&bg), ph);
        let hidden = if slide.hidden { r#" presentation:visibility="hidden""# } else { "" };
        let dp = self.style('d', "drawing-page", &format!(r#"<style:drawing-page-properties{fill}{hidden} presentation:background-visible="true" presentation:background-objects-visible="true"/>"#));
        let name = if slide.name.is_empty() { format!("page{}", index + 1) } else { slide.name.clone() };
        let _ = write!(out, r#"<draw:page draw:name="{}" draw:style-name="{dp}" draw:master-page-name="Default"{hidden}>"#, esc_attr(&name));
        for sh in &slide.shapes {
            let x = resolve::xfrm(&ctx, sh);
            self.shape(&ctx, sh, x, Affine::IDENTITY, out, 0);
        }
        if !slide.notes.is_empty() {
            let text = self.text(&ctx, None, &slide.notes);
            let _ = write!(
                out,
                r#"<presentation:notes><draw:frame presentation:class="notes" svg:x="54pt" svg:y="396pt" svg:width="432pt" svg:height="288pt"><draw:text-box>{text}</draw:text-box></draw:frame></presentation:notes>"#
            );
        }
        out.push_str("</draw:page>");
    }

    /// Where a box goes: `svg:x/y/width/height`, or a `draw:transform` for a turned one.
    fn geometry_attrs(x: Xfrm, to_page: Affine) -> String {
        let p0 = to_page * Point::new(x.x, x.y);
        let p1 = to_page * Point::new(x.x + x.w, x.y + x.h);
        let (w, h) = ((p1.x - p0.x).abs(), (p1.y - p0.y).abs());
        let (x0, y0) = (p0.x.min(p1.x), p0.y.min(p1.y));
        if x.rot == 0.0 || !x.rot.is_finite() {
            return format!(r#" svg:x="{}" svg:y="{}" svg:width="{}" svg:height="{}""#, pt(x0), pt(y0), pt(w), pt(h));
        }
        // rotate (θ) then translate: θ counter-clockwise in radians, the box's centre kept.
        let theta = -x.rot.to_radians();
        let (s, c) = theta.sin_cos();
        let (cx, cy) = (x0 + w / 2.0, y0 + h / 2.0);
        let (hx, hy) = (w / 2.0, h / 2.0);
        let tx = cx - (hx * c + hy * s);
        let ty = cy - (-hx * s + hy * c);
        format!(r#" svg:width="{}" svg:height="{}" draw:transform="rotate ({}) translate ({} {})""#, pt(w), pt(h), theta, pt(tx), pt(ty))
    }

    fn shape(&mut self, ctx: &Ctx, sh: &Shape, x: Xfrm, to_page: Affine, out: &mut String, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let name = if sh.name.is_empty() { String::new() } else { format!(r#" draw:name="{}""#, esc_attr(&sh.name)) };
        let desc = if sh.descr.is_empty() { String::new() } else { format!("<svg:desc>{}</svg:desc>", esc(&sh.descr)) };
        match &sh.kind {
            ShapeKind::Group { children, child } => {
                // Child space → the group's box on the page.
                let sx = if child.w.abs() > 1e-9 { x.w / child.w } else { 1.0 };
                let sy = if child.h.abs() > 1e-9 { x.h / child.h } else { 1.0 };
                let inner = to_page * Affine::translate((x.x, x.y)) * Affine::scale_non_uniform(sx, sy) * Affine::translate((-child.x, -child.y));
                let _ = write!(out, "<draw:g{name}>");
                for c in children {
                    let cx = c.xfrm.unwrap_or_default();
                    self.shape(ctx, c, cx, inner, out, depth + 1);
                }
                out.push_str("</draw:g>");
            }
            ShapeKind::Table(t) => {
                let g = Self::geometry_attrs(x, to_page);
                let table = self.table(ctx, sh, t);
                let _ = write!(out, "<draw:frame{name}{g}>{table}{desc}</draw:frame>");
            }
            ShapeKind::Picture { fill } => self.picture_frame(fill.media, sh, x, to_page, &name, &desc, out),
            ShapeKind::Opaque { preview: Some(m), .. } | ShapeKind::Model3d { poster: Some(m), .. } => {
                self.picture_frame(*m, sh, x, to_page, &name, &desc, out)
            }
            ShapeKind::Media(clip) => {
                if let Some(m) = clip.poster {
                    self.picture_frame(m, sh, x, to_page, &name, &desc, out);
                }
            }
            ShapeKind::Chart(_) | ShapeKind::Ink { .. } | ShapeKind::Opaque { .. } | ShapeKind::Model3d { .. } => {}
            ShapeKind::Connector { .. } => self.line_shape(ctx, sh, x, to_page, &name, out),
            ShapeKind::Shape if matches!(&sh.geom, Geom::Preset { name, .. } if deckcraft_geom::preset::is_line_like(name)) => {
                self.line_shape(ctx, sh, x, to_page, &name, out)
            }
            ShapeKind::Shape => self.drawn(ctx, sh, x, to_page, &name, &desc, out),
        }
    }

    fn picture_frame(&mut self, media: MediaId, _sh: &Shape, x: Xfrm, to_page: Affine, name: &str, desc: &str, out: &mut String) {
        let Some(part) = self.picture(media) else { return };
        let g = Self::geometry_attrs(x, to_page);
        let _ = write!(
            out,
            r#"<draw:frame{name}{g}><draw:image xlink:href="{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>{desc}</draw:frame>"#,
            esc_attr(&part)
        );
    }

    fn line_shape(&mut self, ctx: &Ctx, sh: &Shape, x: Xfrm, to_page: Affine, name: &str, out: &mut String) {
        let (mut a, mut b) = (Point::new(x.x, x.y), Point::new(x.x + x.w, x.y + x.h));
        if x.flip_h {
            std::mem::swap(&mut a.x, &mut b.x);
        }
        if x.flip_v {
            std::mem::swap(&mut a.y, &mut b.y);
        }
        let (a, b) = (to_page * a, to_page * b);
        let (line, ph) = resolve::line(ctx, sh);
        let st = self.style('g', "graphic", &format!("<style:graphic-properties{} draw:fill=\"none\"/>", Self::line_attrs(ctx, &line, ph)));
        let tag = if matches!(sh.kind, ShapeKind::Connector { .. }) { "draw:connector" } else { "draw:line" };
        let _ = write!(
            out,
            r#"<{tag}{name} draw:style-name="{st}" svg:x1="{}" svg:y1="{}" svg:x2="{}" svg:y2="{}"/>"#,
            pt(a.x),
            pt(a.y),
            pt(b.x),
            pt(b.y)
        );
    }

    /// A placeholder or text box as a text frame; any other shape as a custom shape.
    fn drawn(&mut self, ctx: &Ctx, sh: &Shape, x: Xfrm, to_page: Affine, name: &str, desc: &str, out: &mut String) {
        let (fill, fph) = resolve::fill(ctx, sh);
        let fill = self.fill_attrs(ctx, fill.as_ref(), fph);
        let (line, lph) = resolve::line(ctx, sh);
        let line = Self::line_attrs(ctx, &line, lph);
        let bp = resolve::body(ctx, sh);
        let mut props = format!("{fill}{line}");
        for (side, v) in [
            ("left", bp.inset_l.unwrap_or(7.2)),
            ("right", bp.inset_r.unwrap_or(7.2)),
            ("top", bp.inset_t.unwrap_or(3.6)),
            ("bottom", bp.inset_b.unwrap_or(3.6)),
        ] {
            let _ = write!(props, r#" fo:padding-{side}="{}""#, pt(v));
        }
        let anchor = match bp.anchor.unwrap_or(Anchor::Top) {
            Anchor::Top => "top",
            Anchor::Middle => "middle",
            Anchor::Bottom => "bottom",
            Anchor::Justified | Anchor::Distributed => "justify",
        };
        let _ = write!(props, r#" draw:textarea-vertical-align="{anchor}" draw:auto-grow-height="false" draw:auto-grow-width="false""#);
        if bp.wrap == Some(false) {
            props.push_str(r#" fo:wrap-option="no-wrap""#);
        }
        let st = self.style('g', "graphic", &format!("<style:graphic-properties{props}/>"));
        let g = Self::geometry_attrs(x, to_page);
        let text = sh.text.as_ref().map(|t| self.text(ctx, Some(sh), t)).unwrap_or_default();
        let class = sh.ph.as_ref().and_then(|ph| match ph.kind {
            PhType::Title | PhType::CtrTitle => Some("title"),
            PhType::SubTitle => Some("subtitle"),
            PhType::Body | PhType::Obj => Some("outline"),
            _ => None,
        });
        let geom_is_rect = matches!(&sh.geom, Geom::Preset { name, .. } if name == "rect");
        if class.is_some() || (sh.text_box && geom_is_rect) {
            let class_attr = class.map(|c| format!(r#" presentation:class="{c}""#)).unwrap_or_default();
            let empty = sh.text.as_ref().is_none_or(TextBody::is_empty);
            let placeholder = if class.is_some() && empty { r#" presentation:placeholder="true""# } else { "" };
            let _ = write!(
                out,
                r#"<draw:frame{name} draw:style-name="{st}"{class_attr}{placeholder}{g}><draw:text-box>{text}</draw:text-box>{desc}</draw:frame>"#
            );
            return;
        }
        let geometry = self.enhanced_geometry(sh, x);
        let _ = write!(out, r#"<draw:custom-shape{name} draw:style-name="{st}"{g}>{desc}{text}{geometry}</draw:custom-shape>"#);
    }

    /// `draw:enhanced-geometry`: the preset's name (`ooxml-<preset>`) and its outline at the
    /// shape's size as a plain path, so readers that don't know the preset still draw it.
    fn enhanced_geometry(&self, sh: &Shape, x: Xfrm) -> String {
        let (w, h) = (x.w.max(0.0), x.h.max(0.0));
        let mut d = String::new();
        let mut kind = String::from("non-primitive");
        let scale = 100.0;
        let push = |d: &mut String, p: &[PathEl], fill: bool, stroke: bool| {
            for el in p {
                match el {
                    PathEl::MoveTo(a) => {
                        let _ = write!(d, "M {} {} ", (a.x * scale).round(), (a.y * scale).round());
                    }
                    PathEl::LineTo(a) => {
                        let _ = write!(d, "L {} {} ", (a.x * scale).round(), (a.y * scale).round());
                    }
                    PathEl::QuadTo(a, b) => {
                        let _ =
                            write!(d, "Q {} {} {} {} ", (a.x * scale).round(), (a.y * scale).round(), (b.x * scale).round(), (b.y * scale).round());
                    }
                    PathEl::CurveTo(a, b, c) => {
                        let _ = write!(
                            d,
                            "C {} {} {} {} {} {} ",
                            (a.x * scale).round(),
                            (a.y * scale).round(),
                            (b.x * scale).round(),
                            (b.y * scale).round(),
                            (c.x * scale).round(),
                            (c.y * scale).round()
                        );
                    }
                    PathEl::ClosePath => d.push_str("Z "),
                }
            }
            if !fill {
                d.push_str("F ");
            }
            if !stroke {
                d.push_str("S ");
            }
            d.push_str("N ");
        };
        match &sh.geom {
            Geom::Preset { name, adj } => {
                if deckcraft_geom::preset::info(name).is_some() {
                    kind = format!("ooxml-{name}");
                }
                let geo = deckcraft_geom::preset::build(name, w, h, adj).or_else(|| deckcraft_geom::preset::build("rect", w, h, &[]));
                for sp in geo.map(|g| g.paths).unwrap_or_default() {
                    let fill = !matches!(sp.fill, deckcraft_geom::preset::FillMode::None);
                    push(&mut d, sp.path.elements(), fill, sp.stroke);
                }
            }
            Geom::Custom { paths } => {
                for cp in paths {
                    let (sx, sy) = (if cp.w > 0.0 { w / cp.w } else { 1.0 }, if cp.h > 0.0 { h / cp.h } else { 1.0 });
                    let path = custom_path(&cp.d, sx, sy);
                    push(&mut d, &path, !matches!(cp.fill, deckcraft_geom::preset::FillMode::None), cp.stroke);
                }
            }
        }
        let mirror = format!(
            "{}{}",
            if x.flip_h { r#" draw:mirror-horizontal="true""# } else { "" },
            if x.flip_v { r#" draw:mirror-vertical="true""# } else { "" }
        );
        format!(
            r#"<draw:enhanced-geometry svg:viewBox="0 0 {} {}" draw:type="{}"{mirror} draw:enhanced-path="{}"/>"#,
            (w * scale).round(),
            (h * scale).round(),
            esc_attr(&kind),
            esc_attr(d.trim_end())
        )
    }

    fn table(&mut self, ctx: &Ctx, sh: &Shape, t: &Table) -> String {
        let mut s = String::from("<table:table>");
        for w in &t.cols {
            let co = self.style('c', "table-column", &format!(r#"<style:table-column-properties style:column-width="{}"/>"#, pt(*w)));
            let _ = write!(s, r#"<table:table-column table:style-name="{co}"/>"#);
        }
        for row in &t.rows {
            let ro = self.style('r', "table-row", &format!(r#"<style:table-row-properties style:row-height="{}"/>"#, pt(row.height)));
            let _ = write!(s, r#"<table:table-row table:style-name="{ro}">"#);
            for cell in &row.cells {
                if cell.is_covered() {
                    s.push_str("<table:covered-table-cell/>");
                    continue;
                }
                let mut props = self.fill_attrs(ctx, cell.fill.as_ref(), None);
                if let Some([l, r, tp, b]) = cell.margins {
                    let _ = write!(
                        props,
                        r#" fo:padding-left="{}" fo:padding-right="{}" fo:padding-top="{}" fo:padding-bottom="{}""#,
                        pt(l),
                        pt(r),
                        pt(tp),
                        pt(b)
                    );
                }
                if let Some(a) = cell.anchor {
                    let v = match a {
                        Anchor::Top => "top",
                        Anchor::Middle => "middle",
                        _ => "bottom",
                    };
                    let _ = write!(props, r#" draw:textarea-vertical-align="{v}""#);
                }
                let mut borders = String::new();
                for (side, l) in ["left", "right", "top", "bottom"].iter().zip(&cell.borders) {
                    let Some(l) = l else { continue };
                    let v = match &l.fill {
                        Some(Fill::Solid { color }) => format!("{} solid {}", pt(l.width.unwrap_or(0.75)), hex(ctx.color(color, None))),
                        _ => "none".into(),
                    };
                    let _ = write!(borders, r#" fo:border-{side}="{v}""#);
                }
                let ce = self.style('e', "table-cell", &format!("<style:graphic-properties{props}/><style:paragraph-properties{borders}/>"));
                let span = if cell.grid_span > 1 || cell.row_span > 1 {
                    format!(r#" table:number-columns-spanned="{}" table:number-rows-spanned="{}""#, cell.grid_span.max(1), cell.row_span.max(1))
                } else {
                    String::new()
                };
                let text = self.text(ctx, Some(sh), &cell.text);
                let _ = write!(s, r#"<table:table-cell table:style-name="{ce}"{span}>{text}</table:table-cell>"#);
            }
            s.push_str("</table:table-row>");
        }
        s.push_str("</table:table>");
        s
    }

    // ------------------------------------------------------------------------------------- text

    /// The paragraphs of `body`, styles resolved through `shape`'s placeholder chain (or the
    /// master's other style for notes).
    fn text(&mut self, ctx: &Ctx, shape: Option<&Shape>, body: &TextBody) -> String {
        let blank = Shape::default();
        let shape = shape.unwrap_or(&blank);
        let mut out = String::new();
        for para in &body.paragraphs {
            let pp = resolve::para(ctx, shape, para);
            let mut props = String::new();
            if let Some(a) = pp.align {
                let v = match a {
                    Align::Left => "start",
                    Align::Center => "center",
                    Align::Right => "end",
                    Align::Justify | Align::JustLow | Align::Distributed => "justify",
                };
                let _ = write!(props, r#" fo:text-align="{v}""#);
            }
            if let Some(m) = pp.margin_left {
                let _ = write!(props, r#" fo:margin-left="{}""#, pt(m));
            }
            if let Some(m) = pp.margin_right {
                let _ = write!(props, r#" fo:margin-right="{}""#, pt(m));
            }
            if let Some(i) = pp.indent {
                let _ = write!(props, r#" fo:text-indent="{}""#, pt(i));
            }
            if let Some(Spacing::Pts(v)) = pp.space_before {
                let _ = write!(props, r#" fo:margin-top="{}""#, pt(v));
            }
            if let Some(Spacing::Pts(v)) = pp.space_after {
                let _ = write!(props, r#" fo:margin-bottom="{}""#, pt(v));
            }
            match pp.line_spacing {
                Some(Spacing::Pct(v)) => {
                    let _ = write!(props, r#" fo:line-height="{}%""#, num(v * 100.0));
                }
                Some(Spacing::Pts(v)) => {
                    let _ = write!(props, r#" fo:line-height="{}""#, pt(v));
                }
                None => {}
            }
            if pp.rtl == Some(true) {
                props.push_str(r#" style:writing-mode="rl-tb""#);
            }
            let ps = if props.is_empty() { None } else { Some(self.style('P', "paragraph", &format!("<style:paragraph-properties{props}/>"))) };
            let mut p = match &ps {
                Some(n) => format!(r#"<text:p text:style-name="{n}">"#),
                None => "<text:p>".to_string(),
            };
            for run in &para.runs {
                let rp = resolve::run(ctx, shape, para, &run.props);
                let ts = self.run_style(ctx, &rp);
                let content = match &run.kind {
                    RunKind::Break => "<text:line-break/>".to_string(),
                    RunKind::Field { field } if field == "slidenum" => format!("<text:page-number>{}</text:page-number>", esc(&run.text)),
                    _ => para_text(&run.text),
                };
                let content = match &rp.link {
                    Some(h) => match &h.action {
                        deckcraft_model::text::Action::Url { url } => {
                            format!(r#"<text:a xlink:type="simple" xlink:href="{}">{content}</text:a>"#, esc_attr(url))
                        }
                        _ => content,
                    },
                    None => content,
                };
                let _ = write!(p, r#"<text:span text:style-name="{ts}">{content}</text:span>"#);
            }
            p.push_str("</text:p>");
            // A level or a bullet: the paragraph in nested lists, a list header when it has no bullet.
            let bulleted = !matches!(pp.bullet, None | Some(Bullet::None));
            if para.level > 0 || bulleted {
                let item = if bulleted { "text:list-item" } else { "text:list-header" };
                let depth = usize::from(para.level.min(8)) + 1;
                for _ in 0..depth {
                    let _ = write!(out, "<text:list><{item}>");
                }
                out.push_str(&p);
                for _ in 0..depth {
                    let _ = write!(out, "</{item}></text:list>");
                }
            } else {
                out.push_str(&p);
            }
        }
        out
    }

    fn run_style(&mut self, ctx: &Ctx, rp: &RunProps) -> String {
        let mut props = String::new();
        let family = resolve::font_family(ctx, rp);
        if !family.is_empty() {
            let _ = write!(props, r#" fo:font-family="{}""#, esc_attr(&family));
        }
        if let Some(sz) = rp.size {
            let _ = write!(props, r#" fo:font-size="{}""#, pt(sz));
        }
        if rp.bold == Some(true) {
            props.push_str(r#" fo:font-weight="bold""#);
        }
        if rp.italic == Some(true) {
            props.push_str(r#" fo:font-style="italic""#);
        }
        if rp.underline.as_deref().is_some_and(|u| u != "none") {
            props.push_str(r#" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color""#);
        }
        if matches!(rp.strike, Some(Strike::Single | Strike::Double)) {
            props.push_str(r#" style:text-line-through-style="solid""#);
        }
        if let Some(b) = rp.baseline.filter(|b| *b != 0.0) {
            let _ = write!(props, r#" style:text-position="{}% 58%""#, num(b * 100.0));
        }
        let _ = write!(props, r#" fo:color="{}""#, hex(resolve::text_color(ctx, rp)));
        self.style('T', "text", &format!("<style:text-properties{props}/>"))
    }
}

/// A custom path's `M L C Q Z` commands, scaled.
fn custom_path(d: &str, sx: f64, sy: f64) -> Vec<PathEl> {
    let mut out = vec![];
    let it = d.split_whitespace();
    let mut cmd = "M";
    let mut nums: Vec<f64> = vec![];
    let flush = |cmd: &str, nums: &mut Vec<f64>, out: &mut Vec<PathEl>| {
        let pt = |i: usize| Point::new(nums.get(i).copied().unwrap_or(0.0) * sx, nums.get(i + 1).copied().unwrap_or(0.0) * sy);
        match (cmd, nums.len()) {
            ("M", 2) => out.push(PathEl::MoveTo(pt(0))),
            ("L", 2) => out.push(PathEl::LineTo(pt(0))),
            ("Q", 4) => out.push(PathEl::QuadTo(pt(0), pt(2))),
            ("C", 6) => out.push(PathEl::CurveTo(pt(0), pt(2), pt(4))),
            _ => return false,
        }
        nums.clear();
        true
    };
    for t in it {
        if let Ok(v) = t.parse::<f64>() {
            nums.push(v);
            if flush(cmd, &mut nums, &mut out) && cmd == "M" {
                cmd = "L";
            }
            continue;
        }
        nums.clear();
        match t {
            "Z" | "z" => out.push(PathEl::ClosePath),
            "M" | "L" | "C" | "Q" => cmd = t,
            _ => {}
        }
    }
    out
}

/// Text for a `text:span`: runs of spaces after the first are `text:s`, tabs `text:tab`.
fn para_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut spaces = 0usize;
    let mut start = true;
    let flush = |out: &mut String, spaces: &mut usize, start: bool| {
        if *spaces == 0 {
            return;
        }
        let extra = if start {
            *spaces
        } else {
            out.push(' ');
            *spaces - 1
        };
        if extra == 1 {
            out.push_str("<text:s/>");
        } else if extra > 1 {
            let _ = write!(out, r#"<text:s text:c="{extra}"/>"#);
        }
        *spaces = 0;
    };
    for c in s.chars() {
        match c {
            ' ' => spaces += 1,
            '\t' => {
                flush(&mut out, &mut spaces, start);
                out.push_str("<text:tab/>");
                start = false;
            }
            '\u{b}' | '\n' => {
                flush(&mut out, &mut spaces, start);
                out.push_str("<text:line-break/>");
                start = true;
            }
            c => {
                flush(&mut out, &mut spaces, start);
                start = false;
                let mut b = [0u8; 4];
                out.push_str(&esc(c.encode_utf8(&mut b)));
            }
        }
    }
    if spaces > 0 {
        let _ = write!(out, r#"<text:s text:c="{spaces}"/>"#);
    }
    out
}
