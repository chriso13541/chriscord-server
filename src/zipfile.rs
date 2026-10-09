//! A small zip writer for "Download all": puts files into a .zip as-is
//! (the "stored" method, no compression).
//!
//! No compression on purpose. Attachments are mostly photos, videos, audio
//! and archives that are already compressed, so deflating them again would
//! burn CPU on the server for almost no saving. Stored, building a zip is
//! basically a file copy plus a checksum.
//!
//! Handles Zip64, so archives (and files in them) over 4 GB work. Only the
//! standard library is used, which keeps it testable on its own:
//! `rustc --edition 2021 --test src/zipfile.rs && ./zipfile`.

use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const EOCD64_LOCATOR_SIG: u32 = 0x0706_4b50;
const FLAG_UTF8: u16 = 1 << 11; // names are UTF-8
const VERSION_PLAIN: u16 = 20;
const VERSION_ZIP64: u16 = 45;

struct Entry {
    name: Vec<u8>,
    crc: u32,
    size: u64,
    offset: u64,
    dos_time: u16,
    dos_date: u16,
}

pub struct ZipWriter {
    out: BufWriter<File>,
    pos: u64,
    entries: Vec<Entry>,
    /// Sizes/offsets at or above this get Zip64 fields. Always 0xFFFFFFFF
    /// in real use; lowered by the tests to exercise Zip64 without
    /// writing 4 GB.
    zip64_at: u64,
}

impl ZipWriter {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self { out: BufWriter::with_capacity(1 << 20, File::create(path)?), pos: 0, entries: Vec::new(), zip64_at: 0xFFFF_FFFF })
    }

    /// Adds the file at `src` to the zip as `name`. `dos_time`/`dos_date`
    /// are the modified time shown in the zip (see `dos_datetime`).
    pub fn add_file(&mut self, name: &str, src: &Path, dos_time: u16, dos_date: u16) -> io::Result<()> {
        let mut input = File::open(src)?;
        let size = input.metadata()?.len();
        let name = name.as_bytes().to_vec();
        let offset = self.pos;
        let zip64 = size >= self.zip64_at;

        // Local header. The sizes are known up front (stored = copied as is);
        // the CRC isn't until the data has gone through, so it's written as
        // 0 and patched afterwards.
        let mut h = Vec::with_capacity(30 + name.len() + 20);
        put32(&mut h, LOCAL_SIG);
        put16(&mut h, if zip64 { VERSION_ZIP64 } else { VERSION_PLAIN });
        put16(&mut h, FLAG_UTF8);
        put16(&mut h, 0); // method: stored
        put16(&mut h, dos_time);
        put16(&mut h, dos_date);
        put32(&mut h, 0); // crc, patched below
        let small = if zip64 { 0xFFFF_FFFF } else { size as u32 };
        put32(&mut h, small); // compressed size
        put32(&mut h, small); // uncompressed size
        put16(&mut h, name.len() as u16);
        put16(&mut h, if zip64 { 20 } else { 0 });
        h.extend_from_slice(&name);
        if zip64 {
            put16(&mut h, 0x0001);
            put16(&mut h, 16);
            put64(&mut h, size); // uncompressed
            put64(&mut h, size); // compressed
        }
        self.write(&h)?;

        let mut crc = Crc32::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut copied = 0u64;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 { break; }
            crc.update(&buf[..n]);
            self.write(&buf[..n])?;
            copied += n as u64;
        }
        if copied != size {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file changed size while being zipped"));
        }
        let crc = crc.finish();

        // Patch the CRC into the local header (14 bytes in).
        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(offset + 14))?;
        file.write_all(&crc.to_le_bytes())?;
        file.seek(SeekFrom::Start(self.pos))?;

        self.entries.push(Entry { name, crc, size, offset, dos_time, dos_date });
        Ok(())
    }

    /// Writes the central directory and closes the file.
    pub fn finish(mut self) -> io::Result<()> {
        let cd_start = self.pos;
        let entries = std::mem::take(&mut self.entries);
        for e in &entries {
            let big_size = e.size >= self.zip64_at;
            let big_off = e.offset >= self.zip64_at;
            let mut extra = Vec::new();
            if big_size || big_off {
                let mut fields = Vec::new();
                if big_size { put64(&mut fields, e.size); put64(&mut fields, e.size); }
                if big_off { put64(&mut fields, e.offset); }
                put16(&mut extra, 0x0001);
                put16(&mut extra, fields.len() as u16);
                extra.extend_from_slice(&fields);
            }
            let version = if big_size || big_off { VERSION_ZIP64 } else { VERSION_PLAIN };
            let mut c = Vec::with_capacity(46 + e.name.len() + extra.len());
            put32(&mut c, CENTRAL_SIG);
            put16(&mut c, version); // made by (MS-DOS attributes)
            put16(&mut c, version); // needed to extract
            put16(&mut c, FLAG_UTF8);
            put16(&mut c, 0); // stored
            put16(&mut c, e.dos_time);
            put16(&mut c, e.dos_date);
            put32(&mut c, e.crc);
            let small = if big_size { 0xFFFF_FFFF } else { e.size as u32 };
            put32(&mut c, small);
            put32(&mut c, small);
            put16(&mut c, e.name.len() as u16);
            put16(&mut c, extra.len() as u16);
            put16(&mut c, 0); // comment length
            put16(&mut c, 0); // disk number
            put16(&mut c, 0); // internal attributes
            put32(&mut c, 0); // external attributes
            put32(&mut c, if big_off { 0xFFFF_FFFF } else { e.offset as u32 });
            c.extend_from_slice(&e.name);
            c.extend_from_slice(&extra);
            self.write(&c)?;
        }
        let cd_size = self.pos - cd_start;
        let count = entries.len() as u64;

        let mut end = Vec::new();
        let need64 = count >= 0xFFFF || cd_size >= self.zip64_at || cd_start >= self.zip64_at;
        if need64 {
            let eocd64_at = self.pos;
            put32(&mut end, EOCD64_SIG);
            put64(&mut end, 44); // size of the rest of this record
            put16(&mut end, VERSION_ZIP64);
            put16(&mut end, VERSION_ZIP64);
            put32(&mut end, 0); // this disk
            put32(&mut end, 0); // disk with the central directory
            put64(&mut end, count);
            put64(&mut end, count);
            put64(&mut end, cd_size);
            put64(&mut end, cd_start);
            put32(&mut end, EOCD64_LOCATOR_SIG);
            put32(&mut end, 0);
            put64(&mut end, eocd64_at);
            put32(&mut end, 1); // total disks
        }
        put32(&mut end, EOCD_SIG);
        put16(&mut end, 0);
        put16(&mut end, 0);
        let c16 = if need64 { 0xFFFF } else { count as u16 };
        put16(&mut end, c16);
        put16(&mut end, c16);
        put32(&mut end, if need64 { 0xFFFF_FFFF } else { cd_size as u32 });
        put32(&mut end, if need64 { 0xFFFF_FFFF } else { cd_start as u32 });
        put16(&mut end, 0); // comment length
        self.write(&end)?;

        self.out.flush()?;
        self.out.get_ref().sync_all()
    }

    fn write(&mut self, b: &[u8]) -> io::Result<()> {
        self.out.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
}

/// MS-DOS time and date fields for a zip entry (2-second resolution,
/// years 1980–2107; anything outside is clamped).
pub fn dos_datetime(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> (u16, u16) {
    if year < 1980 { return (0, (1 << 5) | 1); } // 1980-01-01 00:00
    let year = year.min(2107) as u32;
    let time = (hour << 11) | (minute << 5) | (second / 2);
    let date = ((year - 1980) << 9) | (month << 5) | day;
    (time as u16, date as u16)
}

/// Makes `name` safe and unique inside the zip: no folders, nothing that
/// would break out of the extract folder, nothing Windows can't create, and
/// "name (2).ext" for repeats.
pub fn unique_entry_name(name: &str, taken: &mut std::collections::HashSet<String>) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    clean = clean.trim().trim_matches('.').trim().to_string();
    if clean.is_empty() { clean = "file".into(); }
    // Keep names a sane length (by characters, so UTF-8 stays valid).
    if clean.chars().count() > 200 {
        let (stem, ext) = split_ext(&clean);
        let keep = 200usize.saturating_sub(ext.chars().count());
        clean = stem.chars().take(keep).collect::<String>() + &ext;
    }
    let mut candidate = clean.clone();
    let mut n = 2;
    while !taken.insert(candidate.to_lowercase()) {
        let (stem, ext) = split_ext(&clean);
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    candidate
}

fn split_ext(name: &str) -> (String, String) {
    match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.to_string(), String::new()),
    }
}

fn put16(v: &mut Vec<u8>, x: u16) { v.extend_from_slice(&x.to_le_bytes()); }
fn put32(v: &mut Vec<u8>, x: u32) { v.extend_from_slice(&x.to_le_bytes()); }
fn put64(v: &mut Vec<u8>, x: u64) { v.extend_from_slice(&x.to_le_bytes()); }

/// CRC-32 (IEEE), table-driven.
struct Crc32(u32);

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

impl Crc32 {
    fn new() -> Self { Crc32(0xFFFF_FFFF) }
    fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }
    fn finish(&self) -> u32 { !self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn crc_matches_known_value() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.finish(), 0xCBF4_3926);
    }

    #[test]
    fn names_are_cleaned_and_unique() {
        let mut taken = HashSet::new();
        assert_eq!(unique_entry_name("photo.jpg", &mut taken), "photo.jpg");
        assert_eq!(unique_entry_name("Photo.JPG", &mut taken), "Photo (2).JPG");
        assert_eq!(unique_entry_name("photo.jpg", &mut taken), "photo (3).jpg");
        assert_eq!(unique_entry_name("../../etc/passwd", &mut taken), "_.._etc_passwd");
        assert_eq!(unique_entry_name("  ", &mut taken), "file");
        assert_eq!(unique_entry_name("a:b?.txt", &mut taken), "a_b_.txt");
    }

    /// Writes a zip for the external check in the test script
    /// (CHRISCORD_ZIP_TEST_OUT=path; ZIP64=1 forces Zip64 fields).
    #[test]
    fn writes_zip_for_external_check() {
        let Ok(out) = std::env::var("CHRISCORD_ZIP_TEST_OUT") else { return };
        let dir = std::env::temp_dir().join(format!("zt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a"); std::fs::write(&a, b"hello world\n").unwrap();
        let b = dir.join("b"); std::fs::write(&b, vec![7u8; 3_000_000]).unwrap();
        let e = dir.join("e"); std::fs::write(&e, b"").unwrap();
        let mut z = ZipWriter::create(Path::new(&out)).unwrap();
        if std::env::var("ZIP64").is_ok() { z.zip64_at = 10; }
        let (t, d) = dos_datetime(2026, 10, 9, 13, 45, 30);
        let mut taken = HashSet::new();
        for (name, p) in [("hello.txt", &a), ("big.bin", &b), ("hello.txt", &a), ("ünïcødé 写真.txt", &a), ("empty", &e)] {
            z.add_file(&unique_entry_name(name, &mut taken), p, t, d).unwrap();
        }
        z.finish().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
