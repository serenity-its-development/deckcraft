use std::io::Write as _;

use deckcraft_model::style::Fill;
use deckcraft_model::text::{Bullet, RunKind};
use deckcraft_model::{Background, Geom, LayoutType, PhType, ShapeKind};

use super::*;

const NS: &str = r##"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""##;

/// A one-pixel PNG.
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn package(content_body: &str, auto_styles: &str) -> Vec<u8> {
    let content = format!(
        r##"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:automatic-styles>{auto_styles}</office:automatic-styles><office:body><office:presentation>{content_body}</office:presentation></office:body></office:document-content>"##
    );
    let styles = format!(
        r##"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:styles><draw:gradient draw:name="Sky" draw:style="linear" draw:start-color="#000080" draw:end-color="#ffffff" draw:angle="0"/><style:default-style style:family="graphic"><style:graphic-properties draw:fill="solid" draw:fill-color="#729fcf" draw:stroke="solid" svg:stroke-color="#3465a4"/></style:default-style><style:style style:name="Default-title" style:family="presentation"><style:graphic-properties draw:fill="none" draw:stroke="none"/><style:text-properties fo:font-size="40pt"/></style:style></office:styles><office:automatic-styles><style:page-layout style:name="PM1"><style:page-layout-properties fo:page-width="28cm" fo:page-height="15.75cm"/></style:page-layout><style:style style:name="Mdp1" style:family="drawing-page"><style:drawing-page-properties draw:fill="gradient" draw:fill-gradient-name="Sky"/></style:style></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="PM1" draw:style-name="Mdp1"/></office:master-styles></office:document-styles>"##
    );
    let meta = format!(
        r##"<?xml version="1.0" encoding="UTF-8"?><office:document-meta {NS}><office:meta><dc:title>Quarterly review</dc:title><meta:initial-creator>Ada</meta:initial-creator></office:meta></office:document-meta>"##
    );
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflated = zip::write::SimpleFileOptions::default();
        z.start_file("mimetype", stored).unwrap();
        z.write_all(MIME_ODP.as_bytes()).unwrap();
        for (name, data) in [
            ("content.xml", content.as_bytes()),
            ("styles.xml", styles.as_bytes()),
            ("meta.xml", meta.as_bytes()),
            ("Pictures/dot.png", PNG),
            ("Pictures/preview.svm", b"VCLMTF"),
        ] {
            z.start_file(name, deflated).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }
    buf.into_inner()
}

const AUTO: &str = r##"<style:style style:name="dp1" style:family="drawing-page"><style:drawing-page-properties draw:fill="solid" draw:fill-color="#ff0000"/></style:style>
<style:style style:name="pr1" style:family="presentation" style:parent-style-name="Default-title"/>
<style:style style:name="gr1" style:family="graphic"><style:graphic-properties draw:fill="solid" draw:fill-color="#00ff00" draw:stroke="none"/></style:style>
<style:style style:name="T1" style:family="text"><style:text-properties fo:font-weight="bold" fo:color="#0000ff" fo:font-size="20pt"/></style:style>
<style:style style:name="P1" style:family="paragraph"><style:paragraph-properties fo:text-align="center"/></style:style>
<style:style style:name="co1" style:family="table-column"><style:table-column-properties style:column-width="5cm"/></style:style>
<style:style style:name="ro1" style:family="table-row"><style:table-row-properties style:row-height="1cm"/></style:style>
<style:style style:name="ce1" style:family="table-cell"><style:graphic-properties draw:fill="solid" draw:fill-color="#66ffff"/></style:style>"##;

fn deck() -> Presentation {
    let body = r##"
<draw:page draw:name="Agenda" draw:style-name="dp1" draw:master-page-name="Default">
  <draw:frame presentation:style-name="pr1" presentation:class="title" svg:x="1cm" svg:y="1cm" svg:width="20cm" svg:height="3cm"><draw:text-box><text:p>Quarterly <text:span text:style-name="T1">results</text:span></text:p></draw:text-box></draw:frame>
  <draw:frame presentation:class="outline" svg:x="1cm" svg:y="5cm" svg:width="20cm" svg:height="8cm"><draw:text-box><text:list><text:list-item><text:p>Revenue<text:s text:c="2"/>up</text:p><text:list><text:list-item><text:p>EMEA</text:p></text:list-item></text:list></text:list-item><text:list-header><text:p>No bullet</text:p></text:list-header></text:list></draw:text-box></draw:frame>
  <draw:custom-shape draw:name="Oval" draw:style-name="gr1" svg:x="2cm" svg:y="2cm" svg:width="4cm" svg:height="2cm"><text:p text:style-name="P1">Inside<text:line-break/>shape</text:p><draw:enhanced-geometry svg:viewBox="0 0 21600 21600" draw:type="ellipse"/></draw:custom-shape>
  <draw:custom-shape svg:x="2cm" svg:y="2cm" svg:width="4cm" svg:height="2cm"><draw:enhanced-geometry svg:viewBox="0 0 21600 21600" draw:type="ooxml-chevron"/></draw:custom-shape>
  <draw:custom-shape svg:x="2cm" svg:y="2cm" svg:width="4cm" svg:height="2cm"><draw:enhanced-geometry svg:viewBox="0 0 100 100" draw:type="non-primitive" draw:enhanced-path="M 0 0 L 100 0 100 100 Z N"/></draw:custom-shape>
  <draw:g><draw:rect svg:x="10cm" svg:y="10cm" svg:width="1cm" svg:height="1cm"/><draw:line svg:x1="3cm" svg:y1="3cm" svg:x2="1cm" svg:y2="4cm"/></draw:g>
  <draw:frame draw:name="Pic" svg:x="0cm" svg:y="0cm" svg:width="2cm" svg:height="2cm"><draw:image xlink:href="Pictures/preview.svm"/><draw:image xlink:href="Pictures/dot.png"/></draw:frame>
  <draw:frame draw:name="Turned" svg:width="2cm" svg:height="1cm" draw:transform="rotate (1.5707963267949) translate (5cm 6cm)"><draw:text-box><text:p>Turned</text:p></draw:text-box></draw:frame>
  <draw:frame draw:name="Table 1" svg:x="1cm" svg:y="14cm" svg:width="10cm" svg:height="2cm"><table:table><table:table-column table:style-name="co1" table:number-columns-repeated="2"/><table:table-row table:style-name="ro1"><table:table-cell table:style-name="ce1" table:number-columns-spanned="2"><text:p>Wide</text:p></table:table-cell><table:covered-table-cell/></table:table-row><table:table-row table:style-name="ro1"><table:table-cell><text:p>a</text:p></table:table-cell><table:table-cell><text:p>b</text:p></table:table-cell></table:table-row></table:table><draw:image xlink:href="Pictures/preview.svm"/></draw:frame>
  <presentation:notes><draw:frame presentation:class="notes"><draw:text-box><text:p>Speak slowly</text:p></draw:text-box></draw:frame></presentation:notes>
</draw:page>
<draw:page draw:name="page2" draw:master-page-name="Default" presentation:visibility="hidden">
  <draw:frame presentation:class="title" presentation:placeholder="true" svg:x="1cm" svg:y="1cm" svg:width="20cm" svg:height="3cm"><draw:text-box/></draw:frame>
  <draw:frame presentation:class="subtitle" svg:x="1cm" svg:y="5cm" svg:width="20cm" svg:height="3cm"><draw:text-box><text:p>By <text:a xlink:href="https://example.com">us</text:a></text:p></draw:text-box></draw:frame>
</draw:page>"##;
    import(&package(body, AUTO)).expect("odp imports")
}

fn by_name<'a>(s: &'a Slide, name: &str) -> &'a Shape {
    s.shapes.iter().find(|x| x.name == name).unwrap_or_else(|| panic!("no shape {name}"))
}

#[test]
fn sniffs_odp_and_not_other_zips() {
    assert!(sniff(&package("", "")));
    assert!(!sniff(b"PK\x03\x04 not a zip"));
    assert!(!sniff(b"<html/>"));
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file("mimetype", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"application/vnd.oasis.opendocument.text").unwrap();
        z.finish().unwrap();
    }
    assert!(!sniff(&buf.into_inner()), "an .odt is not a presentation");
}

#[test]
fn pages_size_and_properties() {
    let p = deck();
    assert_eq!(p.slides.len(), 2);
    assert!((p.slide_size.width - 28.0 * 72.0 / 2.54).abs() < 0.01);
    assert!((p.slide_size.height - 15.75 * 72.0 / 2.54).abs() < 0.01);
    assert_eq!(p.props.title, "Quarterly review");
    assert_eq!(p.props.author, "Ada");
    assert_eq!(p.slides[0].name, "Agenda");
    assert_eq!(p.slides[1].name, "", "a generated page name is not a slide name");
    assert!(p.slides[1].hidden);
    assert!(p.validate().is_empty(), "{:?}", p.validate());
}

#[test]
fn title_and_outline_become_placeholders_on_a_matching_layout() {
    let p = deck();
    let s = &p.slides[0];
    assert_eq!(p.layout(s.layout).map(|(_, l)| l.kind), Some(LayoutType::TitleAndContent));
    assert_eq!(s.title(), "Quarterly results");
    let title = s.shapes.iter().find(|x| x.ph_type().is_some_and(PhType::is_title)).unwrap();
    let runs = &title.text.as_ref().unwrap().paragraphs[0].runs;
    assert_eq!(runs[0].props.size, Some(40.0), "the presentation style's size, through its parent");
    assert_eq!(runs[1].props.bold, Some(true));
    assert_eq!(runs[1].props.size, Some(20.0));
    let body = s.shapes.iter().find(|x| matches!(x.ph_type(), Some(PhType::Body | PhType::Obj))).unwrap();
    let paras = &body.text.as_ref().unwrap().paragraphs;
    assert_eq!(paras.iter().map(|p| p.text()).collect::<Vec<_>>(), ["Revenue  up", "EMEA", "No bullet"]);
    assert_eq!(paras.iter().map(|p| p.level).collect::<Vec<_>>(), [0, 1, 0]);
    assert_eq!(paras[2].props.bullet, Some(Bullet::None), "a list header has no bullet");
    assert_eq!(s.notes_text(), "Speak slowly");
    // The second page: a title and a subtitle make a title slide.
    let s2 = &p.slides[1];
    assert_eq!(p.layout(s2.layout).map(|(_, l)| l.kind), Some(LayoutType::Title));
    let sub = s2.shapes.iter().find(|x| x.ph_type() == Some(PhType::SubTitle)).unwrap();
    assert_eq!(sub.text.as_ref().unwrap().text(), "By us");
    assert!(sub.text.as_ref().unwrap().paragraphs[0].runs[1].props.link.is_some());
}

#[test]
fn backgrounds_from_the_page_then_its_master() {
    let p = deck();
    assert!(matches!(&p.slides[0].background, Some(Background::Fill { fill: Fill::Solid { .. } })));
    assert!(matches!(&p.slides[1].background, Some(Background::Fill { fill: Fill::Gradient(_) })), "{:?}", p.slides[1].background);
}

#[test]
fn drawing_shapes_geometry_text_and_style() {
    let p = deck();
    let s = &p.slides[0];
    let oval = by_name(s, "Oval");
    assert_eq!(oval.geom.preset_name(), Some("ellipse"));
    assert!(matches!(&oval.fill, Some(Fill::Solid { color }) if color.base == deckcraft_model::style::ColorBase::Rgb { rgb: Rgba::rgb(0, 255, 0) }));
    assert!(matches!(&oval.line, Some(l) if l.fill == Some(Fill::None)));
    let t = oval.text.as_ref().unwrap();
    assert_eq!(t.paragraphs[0].runs.iter().filter(|r| r.kind == RunKind::Break).count(), 1);
    assert_eq!(t.text(), "Inside\u{b}shape");
    let presets: Vec<_> = s.shapes.iter().filter_map(|x| x.geom.preset_name()).collect();
    assert!(presets.contains(&"chevron"), "ooxml-<preset> is that preset: {presets:?}");
    assert!(
        s.shapes.iter().any(|x| matches!(&x.geom, Geom::Custom { paths } if paths[0].d == "M 0 0 L 100 0 L 100 100 Z")),
        "a plain enhanced path is kept"
    );
    // A group is flattened; a line keeps its direction as a flip.
    let line = s.shapes.iter().find(|x| x.geom.preset_name() == Some("line")).unwrap();
    let x = line.xfrm.unwrap();
    assert!(x.flip_h && !x.flip_v);
    assert!((x.w - 2.0 * 72.0 / 2.54).abs() < 0.01);
}

#[test]
fn pictures_skip_formats_nothing_draws() {
    let p = deck();
    let pic = by_name(&p.slides[0], "Pic");
    let ShapeKind::Picture { fill } = &pic.kind else { panic!("{:?}", pic.kind) };
    assert_eq!(p.media(fill.media).map(|m| m.content_type.as_str()), Some("image/png"));
    assert_eq!(p.media.len(), 1, "the .svm previews are not kept");
}

#[test]
fn rotation_from_draw_transform() {
    let p = deck();
    let t = by_name(&p.slides[0], "Turned").xfrm.unwrap();
    assert!((t.rot - 270.0).abs() < 1e-6, "a quarter turn counter-clockwise: {}", t.rot);
    // The box's centre: translate + R·(w/2, h/2) with w = 2cm, h = 1cm.
    let cm = 72.0 / 2.54;
    let (cx, cy) = (t.x + t.w / 2.0, t.y + t.h / 2.0);
    assert!((cx - (5.0 + 0.5) * cm).abs() < 0.01, "{cx}");
    assert!((cy - (6.0 - 1.0) * cm).abs() < 0.01, "{cy}");
}

#[test]
fn tables_with_spans_widths_and_fills() {
    let p = deck();
    let tbl = by_name(&p.slides[0], "Table 1");
    let ShapeKind::Table(t) = &tbl.kind else { panic!("a table frame is a table, not its preview picture") };
    assert_eq!(t.cols.len(), 2);
    assert!((t.cols[0] - 5.0 * 72.0 / 2.54).abs() < 0.01);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0].cells[0].grid_span, 2);
    assert!(t.rows[0].cells[1].h_merge);
    assert!(matches!(t.rows[0].cells[0].fill, Some(Fill::Solid { .. })));
    assert_eq!(t.rows[1].cells[1].text.text(), "b");
}

#[test]
fn units_and_paths() {
    assert_eq!(len_pt("1in"), Some(72.0));
    assert_eq!(len_pt("12pt"), Some(12.0));
    assert!((len_pt("2.54cm").unwrap() - 72.0).abs() < 1e-9);
    assert_eq!(len_pt("50%"), None);
    assert_eq!(svg_path_to_model("m10 10 h5 v5 z", 0.0, 0.0).as_deref(), Some("M 10 10 L 15 10 L 15 15 Z"));
    assert_eq!(svg_path_to_model("M0,0 C1,1 2,2 3,3 S5,5 6,6", 0.0, 0.0).as_deref(), Some("M 0 0 C 1 1 2 2 3 3 C 4 4 5 5 6 6"));
    assert_eq!(svg_path_to_model("M10 10 L20 20", 10.0, 10.0).as_deref(), Some("M 0 0 L 10 10"));
    assert!(enhanced_path(&crate::xml::parse(br##"<g draw:enhanced-path="M ?f0 0 L 1 1" svg:viewBox="0 0 10 10"/>"##).unwrap().root).is_none());
}

#[test]
fn not_a_presentation_is_refused_by_name() {
    assert!(import(b"PK\x03\x04").is_err());
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        z.start_file("mimetype", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(MIME_ODP.as_bytes()).unwrap();
        z.finish().unwrap();
    }
    let e = import(&buf.into_inner()).unwrap_err().to_string();
    assert!(e.contains("content.xml"), "{e}");
}
