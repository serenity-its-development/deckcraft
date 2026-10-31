//! `deckcraft-cli convert` writes the format OUT's extension names, or fails without writing.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cli(args: &[&Path]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_deckcraft-cli")).args(args).output().unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("deckcraft-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn convert_rejects_output_formats_it_cannot_write() {
    let dir = temp_dir("convert-unsupported");
    let input = dir.join("in.pptx");
    let out = Command::new(env!("CARGO_BIN_EXE_deckcraft-cli")).args(["run", "--sample", "--export"]).arg(&input).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    for name in ["out.svg2", "out.bmp", "out.svg", "out.outline", "out"] {
        let target = dir.join(name);
        let out = cli(&[Path::new("convert"), &input, &target]);
        assert!(!out.status.success(), "{name}: convert should fail");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("unsupported output format") && err.contains(".pptx"), "{name}: {err}");
        assert!(!target.exists(), "{name}: nothing is written");
    }

    let target = dir.join("out.deckcraft");
    let out = cli(&[Path::new("convert"), &input, &target]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read(&target).unwrap().starts_with(b"PK\x03\x04"));

    let target = dir.join("out.txt");
    let out = cli(&[Path::new("convert"), &input, &target]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!std::fs::read(&target).unwrap().starts_with(b"PK"), "an outline is text, not a zip");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_rejects_unsupported_save_and_export_paths_before_running() {
    let dir = temp_dir("run-unsupported");
    for flag in ["--save", "--export"] {
        let target = dir.join("deck.svg");
        let out = Command::new(env!("CARGO_BIN_EXE_deckcraft-cli")).args(["run", "--sample", flag]).arg(&target).output().unwrap();
        assert!(!out.status.success(), "{flag}: run should fail");
        assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported output format .svg"), "{flag}");
        assert!(!target.exists(), "{flag}: nothing is written");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A small OpenDocument presentation: a title slide and a slide with a title and a list.
fn odp(path: &Path) {
    use std::io::Write as _;
    const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0""#;
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:body><office:presentation>
<draw:page draw:name="page1" draw:master-page-name="Default"><draw:frame presentation:class="title" svg:x="1cm" svg:y="1cm" svg:width="20cm" svg:height="3cm"><draw:text-box><text:p>Harbour plan</text:p></draw:text-box></draw:frame><draw:frame presentation:class="subtitle" svg:x="1cm" svg:y="5cm" svg:width="20cm" svg:height="3cm"><draw:text-box><text:p>Second edition</text:p></draw:text-box></draw:frame></draw:page>
<draw:page draw:name="page2" draw:master-page-name="Default"><draw:frame presentation:class="title" svg:x="1cm" svg:y="1cm" svg:width="20cm" svg:height="3cm"><draw:text-box><text:p>Moorings</text:p></draw:text-box></draw:frame><draw:frame presentation:class="outline" svg:x="1cm" svg:y="5cm" svg:width="20cm" svg:height="9cm"><draw:text-box><text:list><text:list-item><text:p>Pontoons</text:p></text:list-item></text:list></draw:text-box></draw:frame><draw:custom-shape svg:x="22cm" svg:y="5cm" svg:width="4cm" svg:height="4cm"><text:p>Buoy</text:p><draw:enhanced-geometry svg:viewBox="0 0 21600 21600" draw:type="ellipse"/></draw:custom-shape></draw:page>
</office:presentation></office:body></office:document-content>"#
    );
    let styles = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:automatic-styles><style:page-layout style:name="PM1"><style:page-layout-properties fo:page-width="28cm" fo:page-height="21cm"/></style:page-layout></office:automatic-styles><office:master-styles><style:master-page style:name="Default" style:page-layout-name="PM1"/></office:master-styles></office:document-styles>"#
    );
    let f = std::fs::File::create(path).unwrap();
    let mut z = zip::ZipWriter::new(f);
    z.start_file("mimetype", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
    z.write_all(b"application/vnd.oasis.opendocument.presentation").unwrap();
    for (name, data) in [("content.xml", content), ("styles.xml", styles)] {
        z.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(data.as_bytes()).unwrap();
    }
    z.finish().unwrap();
}

/// Corpus D1: `.odp` could not be opened at all, so odp -> pdf / pptx / thumbnail were refused.
#[test]
fn odp_opens_and_converts_to_pdf_pptx_and_outline() {
    let dir = temp_dir("convert-odp");
    let input = dir.join("deck.odp");
    odp(&input);

    let info = cli(&[Path::new("info"), &input]);
    assert!(info.status.success(), "{}", String::from_utf8_lossy(&info.stderr));
    let v: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(v["slides"].as_array().map(Vec::len), Some(2), "{v}");

    let pdf = dir.join("deck.pdf");
    let out = cli(&[Path::new("convert"), &input, &pdf]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read(&pdf).unwrap().starts_with(b"%PDF"));

    let pptx = dir.join("deck.pptx");
    let out = cli(&[Path::new("convert"), &input, &pptx]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let md = dir.join("deck.md");
    let out = cli(&[Path::new("convert"), &pptx, &md]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&md).unwrap();
    for word in ["Harbour plan", "Second edition", "Moorings", "Pontoons"] {
        assert!(text.contains(word), "{word} is in the pptx made from the odp: {text}");
    }

    // And saved as .odp again, after an edit, it reads back with the edit.
    let saved = dir.join("saved.odp");
    let out = Command::new(env!("CARGO_BIN_EXE_deckcraft-cli"))
        .args(["run", "--in"])
        .arg(&input)
        .args(["--cmd", r#"slide.new={"title":"CORPUSEDIT"}"#, "--save"])
        .arg(&saved)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let md = dir.join("saved.md");
    let out = cli(&[Path::new("convert"), &saved, &md]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(&md).unwrap();
    for word in ["Harbour plan", "Moorings", "Pontoons", "Buoy", "CORPUSEDIT"] {
        assert!(text.contains(word), "{word} is in the saved odp: {text}");
    }

    let png = dir.join("deck.png");
    let out = Command::new(env!("CARGO_BIN_EXE_deckcraft-cli"))
        .arg("render")
        .arg(&input)
        .args(["--slide", "1", "--scale", "0.25"])
        .arg(&png)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read(&png).unwrap().starts_with(b"\x89PNG"));
    let _ = std::fs::remove_dir_all(&dir);
}
