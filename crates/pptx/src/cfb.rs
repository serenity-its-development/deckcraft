//! Just enough of the Compound File Binary (CFB/OLE2) container ([MS-CFB]) to tell a
//! password-protected presentation apart and to read a PowerPoint 97-2003 `.ppt`'s streams.
//! PowerPoint wraps an encrypted `.pptx` in a CFB holding `EncryptionInfo` and `EncryptedPackage`
//! streams (ECMA-376 / [MS-OFFCRYPTO]) instead of a zip; a binary `.ppt` is a CFB holding
//! `Current User` and `PowerPoint Document` ([`Compound`]). Every read is bounds-checked and every
//! loop bounded. The directory scan was copied from GridCraft's `crates/xlsx/src/cfb.rs`.

/// The CFB signature at the start of the file.
const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
/// Sector ids at or above this are markers (free, end of chain, FAT/DIFAT sector), not sectors.
const MAX_REG_SECT: u32 = 0xFFFF_FFFA;
/// Directory sectors followed at most (4096 entries with 512-byte sectors).
const MAX_DIR_SECTORS: usize = 1024;
/// DIFAT sectors followed at most.
const MAX_DIFAT_SECTORS: usize = 1024;

/// Stream names that mark an encrypted (password-protected) OOXML package.
const ENCRYPTED_STREAMS: [&str; 2] = ["EncryptedPackage", "EncryptionInfo"];

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    let s = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([*s.first()?, *s.get(1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]))
}

/// Whether `bytes` is a compound file holding an encryption stream. Walks the directory through
/// the FAT; when the structure can't be followed (truncated or malformed), falls back to scanning
/// the bytes for the UTF-16LE stream names.
pub(crate) fn is_encrypted(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&SIGNATURE) {
        return false;
    }
    match directory_names(bytes) {
        Some(names) => names.iter().any(|n| ENCRYPTED_STREAMS.iter().any(|e| n.eq_ignore_ascii_case(e))),
        None => ENCRYPTED_STREAMS.iter().any(|e| contains_utf16(bytes, e)),
    }
}

fn contains_utf16(hay: &[u8], needle: &str) -> bool {
    let n: Vec<u8> = needle.encode_utf16().flat_map(u16::to_le_bytes).collect();
    !n.is_empty() && hay.windows(n.len()).any(|w| w == n.as_slice())
}

/// Names of every directory entry, or `None` when the directory can't be read.
fn directory_names(b: &[u8]) -> Option<Vec<String>> {
    if !b.starts_with(&SIGNATURE) {
        return None;
    }
    let shift = u16_at(b, 30)?;
    if shift != 9 && shift != 12 {
        return None;
    }
    let size = 1usize << shift;
    let per = size / 4;
    let sector = |id: u32| -> Option<&[u8]> {
        let start = (id as usize).checked_add(1)?.checked_mul(size)?;
        b.get(start..start.checked_add(size)?)
    };
    // The DIFAT: the FAT's sector ids, 109 in the header then chained DIFAT sectors.
    let mut difat: Vec<u32> = (0..109).filter_map(|i| u32_at(b, 76 + i * 4)).filter(|&s| s < MAX_REG_SECT).collect();
    let mut next = u32_at(b, 68)?;
    for _ in 0..MAX_DIFAT_SECTORS {
        if next >= MAX_REG_SECT {
            break;
        }
        let s = sector(next)?;
        difat.extend((0..per - 1).filter_map(|i| u32_at(s, i * 4)).filter(|&x| x < MAX_REG_SECT));
        next = u32_at(s, (per - 1) * 4)?;
    }
    let fat_next = |id: u32| -> Option<u32> {
        let idx = id as usize;
        let fat_sector = *difat.get(idx / per)?;
        u32_at(sector(fat_sector)?, (idx % per) * 4)
    };
    let mut names = Vec::new();
    let mut id = u32_at(b, 48)?;
    for _ in 0..MAX_DIR_SECTORS {
        if id == END_OF_CHAIN {
            return Some(names);
        }
        if id >= MAX_REG_SECT {
            return None;
        }
        let s = sector(id)?;
        for e in s.as_chunks::<128>().0 {
            let len = usize::from(u16_at(e, 64)?);
            if !(2..=64).contains(&len) {
                continue;
            }
            let units: Vec<u16> = (0..(len - 2) / 2).filter_map(|i| u16_at(e, i * 2)).collect();
            names.push(String::from_utf16_lossy(&units));
        }
        id = fat_next(id)?;
    }
    // A directory longer than the bound (or a cyclic chain): report what was read.
    Some(names)
}

/// No sibling or child (`NOSTREAM`).
const NO_STREAM: u32 = 0xFFFF_FFFF;
/// Directory entries followed at most.
const MAX_ENTRIES: usize = MAX_DIR_SECTORS * 32;

/// One directory entry: a stream (type 2), a storage (1) or the root (5).
#[derive(Debug, Clone)]
struct Entry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

/// A compound file read for its streams: the FAT, the directory, the mini FAT and the mini stream.
pub(crate) struct Compound<'a> {
    b: &'a [u8],
    shift: u32,
    mini_shift: u32,
    cutoff: u64,
    fat: Vec<u32>,
    mini_fat: Vec<u32>,
    entries: Vec<Entry>,
    mini_stream: Vec<u8>,
}

impl<'a> Compound<'a> {
    /// Read the file's tables, or `None` when it isn't a compound file or its structure can't be
    /// followed.
    pub(crate) fn open(b: &'a [u8]) -> Option<Compound<'a>> {
        if !b.starts_with(&SIGNATURE) {
            return None;
        }
        let shift = u32::from(u16_at(b, 30)?);
        if shift != 9 && shift != 12 {
            return None;
        }
        let mini_shift = u32::from(u16_at(b, 32)?);
        if mini_shift == 0 || mini_shift >= shift {
            return None;
        }
        let major = u16_at(b, 26)?;
        let size = 1usize << shift;
        let per = size / 4;
        let mut c =
            Compound { b, shift, mini_shift, cutoff: u64::from(u32_at(b, 56)?), fat: vec![], mini_fat: vec![], entries: vec![], mini_stream: vec![] };
        // The DIFAT: the FAT's sector ids, 109 in the header then chained DIFAT sectors.
        let mut difat: Vec<u32> = (0..109).filter_map(|i| u32_at(b, 76 + i * 4)).filter(|&s| s < MAX_REG_SECT).collect();
        let mut next = u32_at(b, 68)?;
        for _ in 0..MAX_DIFAT_SECTORS {
            if next >= MAX_REG_SECT {
                break;
            }
            let s = c.sector(next)?;
            difat.extend((0..per - 1).filter_map(|i| u32_at(s, i * 4)).filter(|&x| x < MAX_REG_SECT));
            next = u32_at(s, (per - 1) * 4)?;
        }
        // A FAT can't describe more sectors than the file holds.
        let max_fat = b.len() / 4 + per;
        for id in difat {
            let s = c.sector(id)?;
            c.fat.extend((0..per).filter_map(|i| u32_at(s, i * 4)));
            if c.fat.len() > max_fat {
                return None;
            }
        }
        let dir = c.regular(u32_at(b, 48)?, None)?;
        for e in dir.as_chunks::<128>().0.iter().take(MAX_ENTRIES) {
            let len = usize::from(u16_at(e, 64)?);
            let units: Vec<u16> = if (2..=64).contains(&len) { (0..(len - 2) / 2).filter_map(|i| u16_at(e, i * 2)).collect() } else { vec![] };
            let low = u64::from(u32_at(e, 120)?);
            let size = if major == 4 { low | (u64::from(u32_at(e, 124)?) << 32) } else { low };
            c.entries.push(Entry {
                name: String::from_utf16_lossy(&units),
                kind: *e.get(66)?,
                left: u32_at(e, 68)?,
                right: u32_at(e, 72)?,
                child: u32_at(e, 76)?,
                start: u32_at(e, 116)?,
                size,
            });
        }
        let root = c.entries.first().filter(|r| r.kind == 5)?.clone();
        let first_mini_fat = u32_at(b, 60)?;
        if first_mini_fat < MAX_REG_SECT {
            let m = c.regular(first_mini_fat, None)?;
            c.mini_fat = m.as_chunks::<4>().0.iter().map(|q| u32::from_le_bytes(*q)).collect();
        }
        if root.start < MAX_REG_SECT && root.size > 0 {
            c.mini_stream = c.regular(root.start, Some(root.size))?;
        }
        Some(c)
    }

    fn sector(&self, id: u32) -> Option<&'a [u8]> {
        let size = 1usize << self.shift;
        let start = (id as usize).checked_add(1)?.checked_mul(size)?;
        // The last sector may be cut short by a writer that didn't pad the file.
        let end = start.checked_add(size)?.min(self.b.len());
        self.b.get(start..end).filter(|s| !s.is_empty())
    }

    /// The bytes of the regular-sector chain from `start`, cut to `size` when given. `None` when
    /// the chain is broken, loops, or ends before `size`.
    fn regular(&self, start: u32, size: Option<u64>) -> Option<Vec<u8>> {
        let want = size.map(|s| usize::try_from(s).ok().filter(|&s| s <= self.b.len())).unwrap_or(Some(usize::MAX))?;
        let mut out = Vec::new();
        let mut id = start;
        for _ in 0..=self.fat.len() {
            if id == END_OF_CHAIN || out.len() >= want {
                break;
            }
            if id >= MAX_REG_SECT {
                return None;
            }
            out.extend_from_slice(self.sector(id)?);
            id = *self.fat.get(id as usize)?;
        }
        if id != END_OF_CHAIN && out.len() < want {
            return None; // a chain longer than the FAT: it loops
        }
        if size.is_some() {
            if out.len() < want {
                return None;
            }
            out.truncate(want);
        }
        Some(out)
    }

    /// The bytes of the mini-sector chain from `start`, `size` long.
    fn mini(&self, start: u32, size: u64) -> Option<Vec<u8>> {
        let want = usize::try_from(size).ok().filter(|&s| s <= self.mini_stream.len())?;
        let unit = 1usize << self.mini_shift;
        let mut out = Vec::with_capacity(want);
        let mut id = start;
        for _ in 0..=self.mini_fat.len() {
            if out.len() >= want {
                break;
            }
            if id >= MAX_REG_SECT {
                return None;
            }
            let at = (id as usize).checked_mul(unit)?;
            out.extend_from_slice(self.mini_stream.get(at..at.checked_add(unit)?.min(self.mini_stream.len()))?);
            id = *self.mini_fat.get(id as usize)?;
        }
        if out.len() < want {
            return None;
        }
        out.truncate(want);
        Some(out)
    }

    /// The entries directly in the root storage (not in a sub-storage such as an embedded
    /// object's): the root's child and that child's siblings.
    fn root_children(&self) -> Vec<&Entry> {
        let mut seen = vec![false; self.entries.len()];
        let mut stack = vec![self.entries.first().map_or(NO_STREAM, |r| r.child)];
        let mut out = vec![];
        while let Some(id) = stack.pop() {
            let Some(e) = self.entries.get(id as usize) else { continue };
            let Some(s) = seen.get_mut(id as usize).filter(|s| !**s) else { continue };
            *s = true;
            out.push(e);
            stack.push(e.left);
            stack.push(e.right);
        }
        out
    }

    /// Whether the root storage holds an entry named `name` (case-insensitive, as CFB compares).
    pub(crate) fn has(&self, name: &str) -> bool {
        self.root_children().iter().any(|e| e.name.eq_ignore_ascii_case(name))
    }

    /// The bytes of the stream `name` in the root storage.
    pub(crate) fn stream(&self, name: &str) -> Option<Vec<u8>> {
        let e = self.root_children().into_iter().find(|e| e.kind == 2 && e.name.eq_ignore_ascii_case(name))?;
        if e.size < self.cutoff { self.mini(e.start, e.size) } else { self.regular(e.start, Some(e.size)) }
    }
}

/// A minimal compound-file writer for tests: 512-byte sectors, streams under the mini-stream
/// cutoff in the mini stream, the rest in regular sectors, every entry a sibling of the first.
#[cfg(test)]
pub(crate) fn write(streams: &[(&str, &[u8])]) -> Vec<u8> {
    const SECT: usize = 512;
    const FREE: u32 = 0xFFFF_FFFF;
    let pad = |v: &mut Vec<u8>, to: usize| v.resize(v.len().div_ceil(to) * to, 0);
    // The mini stream and its FAT.
    let mut mini_stream = vec![];
    let mut mini_fat: Vec<u32> = vec![];
    let mut big: Vec<&[u8]> = vec![];
    let mut starts: Vec<(u32, bool)> = vec![];
    for (_, data) in streams {
        if data.len() < 4096 {
            let first = (mini_stream.len() / 64) as u32;
            let n = data.len().div_ceil(64).max(1);
            for k in 0..n {
                mini_fat.push(if k + 1 == n { END_OF_CHAIN } else { first + k as u32 + 1 });
            }
            mini_stream.extend_from_slice(data);
            mini_stream.resize((first as usize + n) * 64, 0);
            starts.push((first, true));
        } else {
            big.push(data);
            starts.push((0, false));
        }
    }
    let mut mini_fat_bytes: Vec<u8> = mini_fat.iter().flat_map(|v| v.to_le_bytes()).collect();
    pad(&mut mini_fat_bytes, SECT);
    let dir_sectors = (streams.len() + 1).div_ceil(4);
    let mini_fat_sectors = mini_fat_bytes.len() / SECT;
    let mini_sectors = mini_stream.len().div_ceil(SECT);
    let big_sectors: Vec<usize> = big.iter().map(|d| d.len().div_ceil(SECT)).collect();
    let others = dir_sectors + mini_fat_sectors + mini_sectors + big_sectors.iter().sum::<usize>();
    let mut fat_sectors = 1;
    while fat_sectors * 128 < fat_sectors + others {
        fat_sectors += 1;
    }
    assert!(fat_sectors <= 109, "test files stay small");
    // Sector layout: FAT, directory, mini FAT, mini stream, big streams.
    let mut fat: Vec<u32> = vec![0xFFFF_FFFD; fat_sectors];
    let chain = |fat: &mut Vec<u32>, n: usize| -> u32 {
        let first = fat.len() as u32;
        for k in 0..n {
            fat.push(if k + 1 == n { END_OF_CHAIN } else { first + k as u32 + 1 });
        }
        if n == 0 { END_OF_CHAIN } else { first }
    };
    let dir_start = chain(&mut fat, dir_sectors);
    let mini_fat_start = chain(&mut fat, mini_fat_sectors);
    let mini_start = chain(&mut fat, mini_sectors);
    let big_starts: Vec<u32> = big_sectors.iter().map(|&n| chain(&mut fat, n)).collect();
    fat.resize(fat_sectors * 128, FREE);

    let mut header = vec![0u8; SECT];
    header[..8].copy_from_slice(&SIGNATURE);
    header[24..26].copy_from_slice(&0x3Eu16.to_le_bytes());
    header[26..28].copy_from_slice(&3u16.to_le_bytes());
    header[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes());
    header[30..32].copy_from_slice(&9u16.to_le_bytes());
    header[32..34].copy_from_slice(&6u16.to_le_bytes());
    header[44..48].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
    header[48..52].copy_from_slice(&dir_start.to_le_bytes());
    header[56..60].copy_from_slice(&4096u32.to_le_bytes());
    header[60..64].copy_from_slice(&mini_fat_start.to_le_bytes());
    header[64..68].copy_from_slice(&(mini_fat_sectors as u32).to_le_bytes());
    header[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
    for i in 0..109 {
        let v = if i < fat_sectors { i as u32 } else { FREE };
        header[76 + i * 4..80 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    let mut dir = vec![0u8; dir_sectors * SECT];
    let mut big_at = 0;
    let entry = |dir: &mut Vec<u8>, k: usize, name: &str, kind: u8, right: u32, child: u32, start: u32, size: usize| {
        let e = k * 128;
        let units: Vec<u16> = name.encode_utf16().take(31).collect();
        for (i, u) in units.iter().enumerate() {
            dir[e + i * 2..e + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        dir[e + 64..e + 66].copy_from_slice(&((units.len() as u16 + 1) * 2).to_le_bytes());
        dir[e + 66] = kind;
        dir[e + 67] = 1; // black
        dir[e + 68..e + 72].copy_from_slice(&FREE.to_le_bytes());
        dir[e + 72..e + 76].copy_from_slice(&right.to_le_bytes());
        dir[e + 76..e + 80].copy_from_slice(&child.to_le_bytes());
        dir[e + 116..e + 120].copy_from_slice(&start.to_le_bytes());
        dir[e + 120..e + 124].copy_from_slice(&(size as u32).to_le_bytes());
    };
    let root_child = if streams.is_empty() { FREE } else { 1 };
    entry(&mut dir, 0, "Root Entry", 5, FREE, root_child, if mini_stream.is_empty() { END_OF_CHAIN } else { mini_start }, mini_stream.len());
    for (k, ((name, data), (mini_first, in_mini))) in streams.iter().zip(&starts).enumerate() {
        let start = if *in_mini {
            *mini_first
        } else {
            big_at += 1;
            big_starts[big_at - 1]
        };
        let right = if k + 1 < streams.len() { k as u32 + 2 } else { FREE };
        entry(&mut dir, k + 1, name, 2, right, FREE, start, data.len());
    }
    for k in streams.len() + 1..dir_sectors * 4 {
        let e = k * 128;
        for at in [68, 72, 76] {
            dir[e + at..e + at + 4].copy_from_slice(&FREE.to_le_bytes());
        }
    }
    let mut out = header;
    out.extend(fat.iter().flat_map(|v| v.to_le_bytes()));
    out.extend_from_slice(&dir);
    out.extend_from_slice(&mini_fat_bytes);
    let mut ms = mini_stream.clone();
    pad(&mut ms, SECT);
    out.extend_from_slice(&ms);
    for d in big {
        let mut d = d.to_vec();
        pad(&mut d, SECT);
        out.extend_from_slice(&d);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// A minimal valid compound file (512-byte sectors: FAT at sector 0, directory at sector 1)
    /// whose directory holds a root entry plus the given stream names.
    fn build(streams: &[&str]) -> Vec<u8> {
        let mut b = vec![0u8; 512 * 3];
        b[..8].copy_from_slice(&SIGNATURE);
        b[24..26].copy_from_slice(&0x3Eu16.to_le_bytes()); // minor version
        b[26..28].copy_from_slice(&3u16.to_le_bytes()); // major version
        b[28..30].copy_from_slice(&0xFFFEu16.to_le_bytes()); // byte order
        b[30..32].copy_from_slice(&9u16.to_le_bytes()); // sector shift
        b[32..34].copy_from_slice(&6u16.to_le_bytes()); // mini sector shift
        b[44..48].copy_from_slice(&1u32.to_le_bytes()); // FAT sectors
        b[48..52].copy_from_slice(&1u32.to_le_bytes()); // first directory sector
        b[60..64].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no mini FAT
        b[68..72].copy_from_slice(&END_OF_CHAIN.to_le_bytes()); // no DIFAT sectors
        for i in 0..109 {
            b[76 + i * 4..80 + i * 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        b[76..80].copy_from_slice(&0u32.to_le_bytes()); // FAT lives in sector 0
        // FAT (sector 0, at byte 512): sector 0 is a FAT sector, sector 1 ends its chain.
        for i in 0..128 {
            b[512 + i * 4..516 + i * 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        }
        b[512..516].copy_from_slice(&0xFFFF_FFFDu32.to_le_bytes());
        b[516..520].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        // Directory (sector 1, at byte 1024).
        for (k, name) in std::iter::once("Root Entry").chain(streams.iter().copied()).take(4).enumerate() {
            let e = 1024 + k * 128;
            let units: Vec<u16> = name.encode_utf16().take(31).collect();
            for (i, u) in units.iter().enumerate() {
                b[e + i * 2..e + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
            }
            b[e + 64..e + 66].copy_from_slice(&((units.len() as u16 + 1) * 2).to_le_bytes());
            b[e + 66] = if k == 0 { 5 } else { 2 };
        }
        b
    }

    #[test]
    fn reads_directory_names() {
        let b = build(&["EncryptionInfo", "EncryptedPackage"]);
        assert_eq!(directory_names(&b), Some(vec!["Root Entry".into(), "EncryptionInfo".into(), "EncryptedPackage".into()]));
        assert!(is_encrypted(&b));
        // A PowerPoint 97-2003 .ppt is a compound file too, without the encryption streams.
        assert!(!is_encrypted(&build(&["PowerPoint Document", "Current User"])));
        assert!(!is_encrypted(b"PK\x03\x04"));
    }

    #[test]
    fn hostile_headers_never_panic() {
        let mut b = build(&["PowerPoint Document"]);
        for at in [30usize, 48, 68, 76, 512, 516, 1024 + 64] {
            for v in [0u32, 1, 7, 0xFFFF_FFFA, 0xFFFF_FFFF, 0x7FFF_FFFF] {
                let mut c = b.clone();
                c[at..at + 4].copy_from_slice(&v.to_le_bytes());
                let _ = is_encrypted(&c);
                for cut in [0usize, 8, 100, 600, 1100] {
                    let _ = is_encrypted(&c[..cut.min(c.len())]);
                }
            }
        }
        // A directory chain that loops back on itself stops at the bound.
        b[516..520].copy_from_slice(&1u32.to_le_bytes());
        assert!(!is_encrypted(&b));
    }

    #[test]
    fn reads_regular_and_mini_streams() {
        let small: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let large: Vec<u8> = (0..9000u32).map(|i| (i * 7) as u8).collect();
        let tiny = b"x".to_vec();
        let b = write(&[("Current User", &small), ("PowerPoint Document", &large), ("Tiny", &tiny)]);
        let c = Compound::open(&b).unwrap();
        assert_eq!(c.stream("current user").unwrap(), small);
        assert_eq!(c.stream("PowerPoint Document").unwrap(), large);
        assert_eq!(c.stream("Tiny").unwrap(), tiny);
        assert!(c.stream("Pictures").is_none());
        assert!(c.has("Tiny") && !c.has("EncryptedSummary"));
        assert_eq!(directory_names(&b).map(|n| n.len()), Some(4));
        // Cut or corrupted files give no stream, never a panic.
        for cut in [0usize, 100, 512, 1500, 2048, b.len() - 1] {
            if let Some(c) = Compound::open(&b[..cut]) {
                let _ = c.stream("PowerPoint Document");
                let _ = c.stream("Current User");
            }
        }
        for at in (0..b.len()).step_by(37) {
            let mut d = b.clone();
            d[at] ^= 0xFF;
            if let Some(c) = Compound::open(&d) {
                let _ = c.stream("PowerPoint Document");
                let _ = c.stream("Current User");
            }
        }
    }

    /// #346: opening a password-protected presentation says so instead of failing as an unknown
    /// file.
    #[test]
    fn a_password_protected_presentation_is_named_as_such() {
        let locked = build(&["EncryptionInfo", "EncryptedPackage"]);
        let err = deckcraft_engine::cmd::file::open_presentation("Locked.pptx", &locked).unwrap_err().to_string();
        assert!(err.contains("Locked.pptx is password-protected"), "{err}");
        let ppt = build(&["PowerPoint Document", "Current User"]);
        let err = deckcraft_engine::cmd::file::open_presentation("Old.ppt", &ppt).unwrap_err().to_string();
        assert!(!err.contains("password"), "{err}");
    }
}
