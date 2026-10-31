//! PowerPoint 97-2003 binary presentations (`.ppt`, `.pps`, `.pot`), read from the public
//! [MS-PPT] specification into the DeckCraft model. Import only: Slides never writes this format,
//! so a `.ppt` is saved as a `.pptx` beside it.
//!
//! What is read: the slide size, every slide in order with its title and body text (paragraphs
//! and line breaks), the text of free text boxes, and the speaker notes. Each slide is built on
//! the Title and Content layout. Pictures, drawings, tables, charts and formatting are not read.
//!
//! The file is a compound file ([`crate::cfb`]). The `Current User` stream names the last
//! `UserEditAtom` in the `PowerPoint Document` stream; the persist directories along that edit
//! chain map persist ids to record offsets, and the `DocumentContainer` holds the
//! `SlideListWithTextContainer` with each slide's placeholder text. Every offset is
//! bounds-checked and every walk bounded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use deckcraft_geom::Size;
use deckcraft_model::text::{Paragraph, Run, RunKind, TextBody};
use deckcraft_model::{LayoutType, PhType, Presentation, defaults};

use crate::PptxError;
use crate::cfb::Compound;

const RT_DOCUMENT: u16 = 0x03E8;
const RT_DOCUMENT_ATOM: u16 = 0x03E9;
const RT_SLIDE: u16 = 0x03EE;
const RT_NOTES: u16 = 0x03F0;
const RT_NOTES_ATOM: u16 = 0x03F1;
const RT_SLIDE_PERSIST_ATOM: u16 = 0x03F3;
const RT_SLIDE_LIST_WITH_TEXT: u16 = 0x0FF0;
const RT_USER_EDIT_ATOM: u16 = 0x0FF5;
const RT_CURRENT_USER_ATOM: u16 = 0x0FF6;
const RT_TEXT_HEADER_ATOM: u16 = 0x0F9F;
const RT_TEXT_CHARS_ATOM: u16 = 0x0FA0;
const RT_TEXT_BYTES_ATOM: u16 = 0x0FA8;
const RT_PERSIST_DIRECTORY_ATOM: u16 = 0x1772;

/// `CurrentUserAtom.headerToken` of an unencrypted and of an encrypted document.
const TOKEN_PLAIN: u32 = 0xE391_C05F;
const TOKEN_ENCRYPTED: u32 = 0xF3D1_C4DF;

/// `TextHeaderAtom.textType`: title, center title, notes.
const TX_TITLE: u32 = 0;
const TX_NOTES: u32 = 2;
const TX_CENTER_TITLE: u32 = 6;

/// `SlideListWithTextContainer` instances: the slides' list and the notes' list.
const LIST_SLIDES: u16 = 0;
const LIST_NOTES: u16 = 2;

/// Edits followed back along the `UserEditAtom` chain at most.
const MAX_EDITS: usize = 4096;
/// Container nesting followed at most when looking for text in a slide's drawing.
const MAX_DEPTH: usize = 32;
/// Master units per point (576 per inch).
const MASTER_PER_PT: f64 = 8.0;

/// Stream names of a binary PowerPoint file.
const DOCUMENT_STREAM: &str = "PowerPoint Document";
const CURRENT_USER_STREAM: &str = "Current User";
/// The stream an encrypted `.ppt` keeps its document summary in ([MS-PPT] 2.3.7).
const ENCRYPTED_SUMMARY: &str = "EncryptedSummary";

/// Is this a PowerPoint 97-2003 binary presentation (a compound file holding the
/// `PowerPoint Document` stream)?
pub fn sniff(bytes: &[u8]) -> bool {
    Compound::open(bytes).is_some_and(|c| c.has(DOCUMENT_STREAM))
}

/// Is this a password-protected `.ppt` (RC4 CryptoAPI encryption: the `EncryptedSummary` stream,
/// the encrypted header token, or an edit naming its `CryptSession10Container`)?
pub fn is_encrypted(bytes: &[u8]) -> bool {
    matches!(read(bytes), Err(Refusal::Encrypted))
}

/// Read a `.ppt` / `.pps` / `.pot` into a presentation.
pub fn import(bytes: &[u8]) -> Result<Presentation, PptxError> {
    match read(bytes) {
        Ok(legacy) => build(legacy),
        Err(Refusal::Encrypted) => Err(PptxError::Encrypted),
        Err(Refusal::Damaged(why)) => Err(PptxError::NotPptx(format!("this PowerPoint 97-2003 file can't be read: {why}"))),
    }
}

#[derive(Debug)]
enum Refusal {
    Encrypted,
    Damaged(&'static str),
}

/// What the file holds, before it becomes a presentation.
#[derive(Debug, Default)]
struct Legacy {
    size: Option<Size>,
    slides: Vec<LegacySlide>,
}

#[derive(Debug, Default)]
struct LegacySlide {
    /// `SlidePersistAtom.slideId`, which a notes page names as its slide.
    id: u32,
    persist: u32,
    title: Vec<String>,
    body: Vec<String>,
    notes: Vec<String>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*s.first()?, *s.get(1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]))
}

/// One record: the header's version, instance and type, and its body.
#[derive(Clone, Copy)]
struct Rec<'a> {
    ver: u16,
    instance: u16,
    kind: u16,
    body: &'a [u8],
}

/// The record at `at`, and where the next one starts.
fn rec_at(b: &[u8], at: usize) -> Option<(Rec<'_>, usize)> {
    let vi = u16_at(b, at)?;
    let kind = u16_at(b, at.checked_add(2)?)?;
    let len = usize::try_from(u32_at(b, at.checked_add(4)?)?).ok()?;
    let start = at.checked_add(8)?;
    let end = start.checked_add(len)?;
    Some((Rec { ver: vi & 0xF, instance: vi >> 4, kind, body: b.get(start..end)? }, end))
}

/// The records in a container's body, in order; a record running past the end stops the list.
fn children(body: &[u8]) -> Vec<Rec<'_>> {
    let mut out = vec![];
    let mut at = 0;
    while at < body.len() {
        let Some((r, next)) = rec_at(body, at) else { break };
        out.push(r);
        at = next;
    }
    out
}

/// The record a persist id points at.
fn persisted<'a>(doc: &'a [u8], persist: &HashMap<u32, u32>, id: u32) -> Option<Rec<'a>> {
    rec_at(doc, usize::try_from(*persist.get(&id)?).ok()?).map(|(r, _)| r)
}

/// A text atom's characters: UTF-16 (`TextCharsAtom`) or the low bytes of UTF-16
/// (`TextBytesAtom`, which is Latin-1).
fn text_of(r: &Rec<'_>) -> Option<String> {
    match r.kind {
        RT_TEXT_CHARS_ATOM => {
            let units: Vec<u16> = r.body.as_chunks::<2>().0.iter().map(|u| u16::from_le_bytes(*u)).collect();
            Some(String::from_utf16_lossy(&units))
        }
        RT_TEXT_BYTES_ATOM => Some(r.body.iter().map(|&b| char::from(b)).collect()),
        _ => None,
    }
}

/// Every (text type, text) pair in a record tree: a `TextHeaderAtom` followed by its text atom,
/// as in a text box's client data (`OfficeArtClientTextbox`).
fn texts_in(r: &Rec<'_>, depth: usize, out: &mut Vec<(u32, String)>) {
    if depth > MAX_DEPTH || r.ver != 0xF {
        return;
    }
    let mut kind = None;
    for c in children(r.body) {
        if c.kind == RT_TEXT_HEADER_ATOM {
            kind = u32_at(c.body, 0);
        } else if let Some(t) = text_of(&c) {
            if let Some(k) = kind.take() {
                out.push((k, t));
            }
        } else {
            texts_in(&c, depth + 1, out);
        }
    }
}

fn read(bytes: &[u8]) -> Result<Legacy, Refusal> {
    let c = Compound::open(bytes).ok_or(Refusal::Damaged("it isn't a compound file"))?;
    if c.has(ENCRYPTED_SUMMARY) {
        return Err(Refusal::Encrypted);
    }
    let user = c.stream(CURRENT_USER_STREAM).ok_or(Refusal::Damaged("the Current User stream is missing"))?;
    let doc = c.stream(DOCUMENT_STREAM).ok_or(Refusal::Damaged("the PowerPoint Document stream is missing"))?;
    let (cu, _) = rec_at(&user, 0).filter(|(r, _)| r.kind == RT_CURRENT_USER_ATOM).ok_or(Refusal::Damaged("the Current User stream is damaged"))?;
    match u32_at(cu.body, 4) {
        Some(TOKEN_ENCRYPTED) => return Err(Refusal::Encrypted),
        Some(TOKEN_PLAIN) => {}
        _ => return Err(Refusal::Damaged("the Current User stream is damaged")),
    }
    let mut edit = u32_at(cu.body, 8).ok_or(Refusal::Damaged("the Current User stream is damaged"))?;

    // The edit chain, newest first: a persist id keeps the offset its newest edit gave it.
    let mut persist: HashMap<u32, u32> = HashMap::new();
    let mut doc_ref = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_EDITS {
        if !seen.insert(edit) {
            break;
        }
        let at = usize::try_from(edit).map_err(|_| Refusal::Damaged("an edit is out of range"))?;
        let (ue, _) = rec_at(&doc, at).filter(|(r, _)| r.kind == RT_USER_EDIT_ATOM).ok_or(Refusal::Damaged("an edit record is missing"))?;
        // The optional last field names the CryptSession10Container: only an encrypted file has it.
        if ue.body.len() >= 0x20 {
            return Err(Refusal::Encrypted);
        }
        let field = |at| u32_at(ue.body, at).ok_or(Refusal::Damaged("an edit record is damaged"));
        let (last, dir_at) = (field(8)?, field(12)?);
        doc_ref = doc_ref.or(Some(field(16)?));
        let dir = usize::try_from(dir_at).ok().and_then(|a| rec_at(&doc, a)).filter(|(r, _)| r.kind == RT_PERSIST_DIRECTORY_ATOM);
        let (dir, _) = dir.ok_or(Refusal::Damaged("a persist directory is missing"))?;
        let mut at = 0usize;
        while let Some(v) = u32_at(dir.body, at) {
            let (first, count) = (v & 0xF_FFFF, (v >> 20) as usize);
            for k in 0..count {
                let Some(offset) = u32_at(dir.body, at + 4 + 4 * k) else { break };
                persist.entry(first.saturating_add(k as u32)).or_insert(offset);
            }
            at = at.saturating_add(4 + 4 * count);
        }
        if last == 0 {
            break;
        }
        edit = last;
    }
    let doc_ref = doc_ref.ok_or(Refusal::Damaged("no edit names the document"))?;
    let document = persisted(&doc, &persist, doc_ref).filter(|r| r.kind == RT_DOCUMENT).ok_or(Refusal::Damaged("the document record is missing"))?;

    let mut legacy = Legacy::default();
    let mut notes: Vec<(u32, Vec<String>)> = vec![];
    for r in children(document.body) {
        match (r.kind, r.instance) {
            (RT_DOCUMENT_ATOM, _) => {
                let (w, h) = (u32_at(r.body, 0).map(|v| v as i32), u32_at(r.body, 4).map(|v| v as i32));
                if let (Some(w), Some(h)) = (w, h)
                    && (8..=40_000).contains(&w)
                    && (8..=40_000).contains(&h)
                {
                    legacy.size = Some(Size::new(f64::from(w) / MASTER_PER_PT, f64::from(h) / MASTER_PER_PT));
                }
            }
            (RT_SLIDE_LIST_WITH_TEXT, LIST_SLIDES) => {
                let mut kind = None;
                for c in children(r.body) {
                    if c.kind == RT_SLIDE_PERSIST_ATOM {
                        let (Some(persist), Some(id)) = (u32_at(c.body, 0), u32_at(c.body, 12)) else { continue };
                        legacy.slides.push(LegacySlide { id, persist, ..Default::default() });
                        kind = None;
                    } else if c.kind == RT_TEXT_HEADER_ATOM {
                        kind = u32_at(c.body, 0);
                    } else if let (Some(t), Some(slide)) = (text_of(&c), legacy.slides.last_mut()) {
                        match kind.take() {
                            Some(TX_TITLE | TX_CENTER_TITLE) => slide.title.push(t),
                            Some(_) => slide.body.push(t),
                            None => {}
                        }
                    }
                }
            }
            (RT_SLIDE_LIST_WITH_TEXT, LIST_NOTES) => {
                // Each notes page names its slide in its NotesAtom; its text is in its drawing,
                // or (from some writers) here after its SlidePersistAtom.
                let mut kind = None;
                for c in children(r.body) {
                    if c.kind == RT_SLIDE_PERSIST_ATOM {
                        let Some(page) = u32_at(c.body, 0).and_then(|p| persisted(&doc, &persist, p)).filter(|p| p.kind == RT_NOTES) else {
                            notes.push((0, vec![]));
                            continue;
                        };
                        let slide = children(page.body).iter().find(|a| a.kind == RT_NOTES_ATOM).and_then(|a| u32_at(a.body, 0)).unwrap_or(0);
                        let mut found = vec![];
                        texts_in(&page, 0, &mut found);
                        notes.push((slide, found.into_iter().filter(|(k, _)| *k == TX_NOTES).map(|(_, t)| t).collect()));
                        kind = None;
                    } else if c.kind == RT_TEXT_HEADER_ATOM {
                        kind = u32_at(c.body, 0);
                    } else if let (Some(t), Some((_, texts))) = (text_of(&c), notes.last_mut())
                        && kind.take() == Some(TX_NOTES)
                        && !texts.contains(&t)
                    {
                        texts.push(t);
                    }
                }
            }
            _ => {}
        }
    }
    // Text boxes that aren't placeholders keep their text in the slide's drawing.
    for slide in &mut legacy.slides {
        let Some(container) = persisted(&doc, &persist, slide.persist).filter(|r| r.kind == RT_SLIDE) else { continue };
        let mut found = vec![];
        texts_in(&container, 0, &mut found);
        for (kind, t) in found {
            if slide.title.contains(&t) || slide.body.contains(&t) || t.trim().is_empty() {
                continue;
            }
            if matches!(kind, TX_TITLE | TX_CENTER_TITLE) && slide.title.is_empty() {
                slide.title.push(t);
            } else {
                slide.body.push(t);
            }
        }
    }
    for (id, texts) in notes {
        if let Some(slide) = legacy.slides.iter_mut().find(|s| s.id == id && id != 0) {
            slide.notes.extend(texts);
        }
    }
    Ok(legacy)
}

/// Paragraphs from PowerPoint text: `\r` ends a paragraph, `\v` is a line break inside one.
fn paragraphs(texts: &[String]) -> Vec<Paragraph> {
    let mut out = vec![];
    for text in texts {
        let text = text.strip_suffix('\r').unwrap_or(text);
        for line in text.split('\r') {
            let mut p = Paragraph::default();
            for (i, part) in line.split('\u{b}').enumerate() {
                if i > 0 {
                    p.runs.push(Run { text: String::new(), props: Default::default(), kind: RunKind::Break });
                }
                if !part.is_empty() {
                    p.runs.push(Run::new(part));
                }
            }
            out.push(p);
        }
    }
    out
}

fn build(legacy: Legacy) -> Result<Presentation, PptxError> {
    // A file without its DocumentAtom gets PowerPoint 97's own default, 10 in × 7.5 in.
    let size = legacy.size.unwrap_or(Size::new(720.0, 540.0));
    let mut p = defaults::blank_presentation(size, Default::default(), false);
    let layout =
        defaults::layout_of_kind(&p, LayoutType::TitleAndContent).ok_or_else(|| PptxError::Unsupported("no Title and Content layout".into()))?;
    for s in legacy.slides {
        // An exhausted id space makes no new slides: what fit is kept (#407).
        let Some(mut slide) = defaults::new_slide(&mut p, layout) else { break };
        for sh in &mut slide.shapes {
            let texts = match sh.ph.as_ref().map(|ph| ph.kind) {
                Some(PhType::Title | PhType::CtrTitle) => &s.title,
                Some(PhType::Body | PhType::Obj | PhType::SubTitle) => &s.body,
                _ => continue,
            };
            if !texts.is_empty() {
                sh.text = Some(TextBody { paragraphs: paragraphs(texts), ..Default::default() });
            }
        }
        if !s.notes.is_empty() {
            slide.notes = TextBody { paragraphs: paragraphs(&s.notes), ..Default::default() };
        }
        p.slides.push(Arc::new(slide));
    }
    Ok(p)
}

/// Builds small `.ppt` files for tests, from [MS-PPT]'s records.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// A slide: title, body (`\r` between paragraphs, `\v` a line break) and notes.
    pub(crate) type Page<'a> = (&'a str, &'a str, &'a str);

    fn rec(ver: u16, instance: u16, kind: u16, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(body.len() + 8);
        out.extend_from_slice(&(ver | (instance << 4)).to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    fn atom(kind: u16, body: &[u8]) -> Vec<u8> {
        rec(0, 0, kind, body)
    }

    fn container(kind: u16, instance: u16, parts: &[Vec<u8>]) -> Vec<u8> {
        rec(0xF, instance, kind, &parts.concat())
    }

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    fn chars(t: &str) -> Vec<u8> {
        atom(RT_TEXT_CHARS_ATOM, &t.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>())
    }

    fn bytes(t: &str) -> Vec<u8> {
        atom(RT_TEXT_BYTES_ATOM, &t.chars().map(|c| c as u8).collect::<Vec<u8>>())
    }

    fn header(kind: u32) -> Vec<u8> {
        atom(RT_TEXT_HEADER_ATOM, &kind.to_le_bytes())
    }

    /// Options for [`build`].
    #[derive(Default, Clone, Copy)]
    pub(crate) struct Options<'a> {
        /// A later fast save that replaced the first slide's title in the slide list.
        pub(crate) retitle: Option<&'a str>,
        /// A free text box (not a placeholder) on the first slide.
        pub(crate) text_box: Option<&'a str>,
        /// Encrypted the RC4 CryptoAPI way: the `EncryptedSummary` stream and header token.
        pub(crate) encrypted: bool,
    }

    /// The `PowerPoint Document` and `Current User` streams of a 4:3 deck, in a compound file.
    pub(crate) fn build(pages: &[Page<'_>], o: Options<'_>) -> Vec<u8> {
        let slide_list = |pages: &[Page<'_>], retitle: Option<&str>| {
            let mut parts = vec![];
            for (i, (title, body, _)) in pages.iter().enumerate() {
                let persist = 2 + i as u32;
                parts.push(atom(RT_SLIDE_PERSIST_ATOM, &u32s(&[persist, 0, 2, 256 + i as u32, 0])));
                let title = if i == 0 { retitle.unwrap_or(title) } else { *title };
                parts.extend([header(TX_TITLE), chars(title), header(1), bytes(body)]);
            }
            container(RT_SLIDE_LIST_WITH_TEXT, LIST_SLIDES, &parts)
        };
        let n = pages.len() as u32;
        let notes_list = container(
            RT_SLIDE_LIST_WITH_TEXT,
            LIST_NOTES,
            &(0..n).map(|i| atom(RT_SLIDE_PERSIST_ATOM, &u32s(&[2 + n + i, 0, 1, 512 + i, 0]))).collect::<Vec<_>>(),
        );
        let document = |list: Vec<u8>| {
            let mut doc_atom = u32s(&[5760, 4320, 4320, 5760, 1, 2, 0, 0]);
            doc_atom.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0]);
            container(RT_DOCUMENT, 0, &[atom(RT_DOCUMENT_ATOM, &doc_atom), list, notes_list.clone()])
        };
        let mut stream = vec![];
        let mut offsets = vec![];
        offsets.push(stream.len() as u32);
        stream.extend(document(slide_list(pages, None)));
        for (i, _) in pages.iter().enumerate() {
            offsets.push(stream.len() as u32);
            let mut drawing = vec![];
            if i == 0
                && let Some(t) = o.text_box
            {
                // PPDrawing › OfficeArtDgContainer › OfficeArtSpContainer › OfficeArtClientTextbox.
                let textbox = container(0xF00D, 0, &[header(4), chars(t)]);
                drawing.push(container(0x040C, 0, &[container(0xF002, 0, &[container(0xF004, 0, &[textbox])])]));
            }
            let mut parts = vec![atom(0x03EF, &[0; 24])];
            parts.extend(drawing);
            stream.extend(container(RT_SLIDE, 0, &parts));
        }
        for (i, (_, _, notes)) in pages.iter().enumerate() {
            offsets.push(stream.len() as u32);
            let mut parts = vec![atom(RT_NOTES_ATOM, &u32s(&[256 + i as u32, 0]))];
            if !notes.is_empty() {
                let textbox = container(0xF00D, 0, &[header(TX_NOTES), chars(notes)]);
                parts.push(container(0x040C, 0, &[container(0xF002, 0, &[container(0xF004, 0, &[textbox])])]));
            }
            stream.extend(container(RT_NOTES, 0, &parts));
        }
        let ids = offsets.len() as u32;
        let dir_at = stream.len() as u32;
        let mut dir = vec![1 | (ids << 20)];
        dir.extend(&offsets);
        stream.extend(atom(RT_PERSIST_DIRECTORY_ATOM, &u32s(&dir)));
        let mut edit_at = stream.len() as u32;
        let user_edit = |last: u32, dir: u32| {
            let mut b = u32s(&[256]);
            b.extend_from_slice(&[0, 0, 0, 3]);
            b.extend(u32s(&[last, dir, 1, ids + 1]));
            b.extend_from_slice(&[1, 0, 0, 0]);
            atom(RT_USER_EDIT_ATOM, &b)
        };
        stream.extend(user_edit(0, dir_at));
        if let Some(t) = o.retitle {
            // A fast save appends the changed document and a directory naming only it.
            let doc_at = stream.len() as u32;
            stream.extend(document(slide_list(pages, Some(t))));
            let dir_at = stream.len() as u32;
            stream.extend(atom(RT_PERSIST_DIRECTORY_ATOM, &u32s(&[1 | (1 << 20), doc_at])));
            let last = edit_at;
            edit_at = stream.len() as u32;
            stream.extend(user_edit(last, dir_at));
        }
        let name = b"Mona";
        let token = if o.encrypted { TOKEN_ENCRYPTED } else { TOKEN_PLAIN };
        let mut cu = u32s(&[0x14, token, edit_at]);
        cu.extend_from_slice(&(name.len() as u16).to_le_bytes());
        cu.extend_from_slice(&0x03F4u16.to_le_bytes());
        cu.extend_from_slice(&[3, 0, 0, 0]);
        cu.extend_from_slice(name);
        cu.extend(u32s(&[8]));
        let current_user = atom(RT_CURRENT_USER_ATOM, &cu);
        let summary = vec![0u8; 64];
        let mut streams: Vec<(&str, &[u8])> = vec![(CURRENT_USER_STREAM, &current_user), (DOCUMENT_STREAM, &stream)];
        if o.encrypted {
            streams.push((ENCRYPTED_SUMMARY, &summary));
        }
        crate::cfb::write(&streams)
    }

    /// The deck the other crates' tests open (`tests/fixtures/plan.ppt`).
    pub(crate) fn plan() -> Vec<u8> {
        build(
            &[
                ("Quarterly plan", "Grow revenue\rHire two engineers\u{b}in Brisbane", "Open with the numbers"),
                ("Risks", "Supply delays\rCaf\u{e9} lease", ""),
            ],
            Options { text_box: Some("Draft only"), ..Default::default() },
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::fixture::{Options, build as ppt, plan};
    use super::*;

    fn body(p: &Presentation, slide: usize, kind: PhType) -> String {
        let s = p.slides.get(slide).unwrap();
        s.shapes
            .iter()
            .find(|sh| sh.ph.as_ref().is_some_and(|ph| ph.kind == kind))
            .and_then(|sh| sh.text.as_ref())
            .map(TextBody::text)
            .unwrap_or_default()
    }

    #[test]
    fn reads_slides_titles_body_and_notes() {
        let bytes = plan();
        assert!(sniff(&bytes) && !is_encrypted(&bytes));
        assert!(!crate::sniff(&bytes), "not a .pptx");
        let p = import(&bytes).unwrap();
        assert_eq!(p.slide_size, Size::new(720.0, 540.0));
        assert_eq!(p.slides.len(), 2);
        assert_eq!(body(&p, 0, PhType::Title), "Quarterly plan");
        assert_eq!(body(&p, 0, PhType::Obj), "Grow revenue\nHire two engineers\u{b}in Brisbane\nDraft only");
        assert_eq!(p.slides[0].notes_text(), "Open with the numbers");
        assert_eq!(body(&p, 1, PhType::Title), "Risks");
        assert_eq!(body(&p, 1, PhType::Obj), "Supply delays\nCaf\u{e9} lease");
        assert_eq!(p.slides[1].notes_text(), "");
        // The line break is a break run, not a character in the text.
        let s = &p.slides[0];
        let b = s.shapes.iter().find(|sh| sh.ph.as_ref().is_some_and(|ph| ph.kind == PhType::Obj)).unwrap();
        let para = &b.text.as_ref().unwrap().paragraphs[1];
        assert_eq!(para.runs.iter().map(|r| r.kind.clone()).collect::<Vec<_>>(), [RunKind::Text, RunKind::Break, RunKind::Text]);
    }

    /// The fixture the engine, UI and CLI tests open is the one [`plan`] builds.
    #[test]
    fn the_fixture_file_is_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/plan.ppt");
        if std::env::var_os("DECKCRAFT_WRITE_FIXTURES").is_some() {
            std::fs::write(path, plan()).unwrap();
        }
        assert_eq!(std::fs::read(path).unwrap(), plan(), "rerun with DECKCRAFT_WRITE_FIXTURES=1");
    }

    /// A fast save appends a new document record; the newest edit's directory wins.
    #[test]
    fn follows_the_edit_chain_to_the_newest_save() {
        let b = ppt(&[("Old title", "Body", "")], Options { retitle: Some("New title"), ..Default::default() });
        let p = import(&b).unwrap();
        assert_eq!(body(&p, 0, PhType::Title), "New title");
        assert_eq!(body(&p, 0, PhType::Obj), "Body");
    }

    #[test]
    fn an_encrypted_file_is_refused_as_password_protected() {
        let b = ppt(&[("Secret", "", "")], Options { encrypted: true, ..Default::default() });
        assert!(sniff(&b) && is_encrypted(&b));
        let err = import(&b).unwrap_err().to_string();
        assert!(err.contains("password-protected"), "{err}");
        let err = deckcraft_engine::cmd::file::open_presentation("Locked.ppt", &b).unwrap_err().to_string();
        assert!(err.contains("Locked.ppt is password-protected"), "{err}");
    }

    #[test]
    fn damaged_files_are_errors_never_panics() {
        let b = plan();
        for cut in [0usize, 8, 600, 1500, 2600, b.len() / 2, b.len() - 1] {
            let _ = import(&b[..cut]);
        }
        for at in (0..b.len()).step_by(7) {
            for x in [0xFFu8, 0x80, 0x01] {
                let mut d = b.clone();
                d[at] ^= x;
                let _ = import(&d);
                let _ = sniff(&d);
            }
        }
        assert!(!sniff(b"PK\x03\x04") && import(b"not a file").is_err());
    }
}
