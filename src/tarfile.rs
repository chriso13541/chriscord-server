//! A small, careful .tar reader for installing plugins (plugins.rs): an
//! admin uploads a plugin as a .tar and it's unpacked into its folder.
//!
//! Only what a plugin needs: regular files and folders, with long names
//! (GNU "L" and pax "path" records), keeping whether a file is executable.
//! Everything else — links, devices, anything that would land outside the
//! folder (absolute paths, "..") — is refused, so an archive can't write
//! anywhere it shouldn't. Only the standard library is used, so it's tested
//! on its own: `rustc --edition 2021 --test src/tarfile.rs && ./tarfile`.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

/// Most bytes one archive may unpack to, and most entries in it.
pub const MAX_TOTAL: u64 = 1 << 30; // 1 GB
pub const MAX_ENTRIES: usize = 20_000;

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// A name inside the archive made safe: relative, no "..", no empty parts.
/// None if it can't be (it would escape the folder).
pub fn safe_path(name: &str) -> Option<PathBuf> {
    let name = name.trim_start_matches("./");
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return None;
    }
    let mut out = PathBuf::new();
    for c in Path::new(name).components() {
        match c {
            Component::Normal(p) => {
                let s = p.to_str()?;
                if s.is_empty() || s.contains(':') { return None; } // (Windows drive letters / streams)
                out.push(s);
            }
            Component::CurDir => {}
            _ => return None, // "..", "/" at the start, a drive prefix
        }
    }
    if out.as_os_str().is_empty() { None } else { Some(out) }
}

/// Octal number field (tar's numbers are octal text, space/NUL padded; a
/// leading 0x80 byte means binary, used for sizes over 8 GB).
fn octal(field: &[u8]) -> io::Result<u64> {
    if field.first().map_or(false, |b| b & 0x80 != 0) {
        let mut v: u64 = (field[0] & 0x7f) as u64;
        for b in &field[1..] {
            v = v.checked_mul(256).and_then(|x| x.checked_add(*b as u64)).ok_or_else(|| invalid("number too big"))?;
        }
        return Ok(v);
    }
    let s: String = field.iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    let s = s.trim();
    if s.is_empty() { return Ok(0); }
    u64::from_str_radix(s, 8).map_err(|_| invalid("bad number in a tar header"))
}

fn text(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// What was unpacked.
#[derive(Debug, Default)]
pub struct Unpacked {
    pub files: usize,
    pub bytes: u64,
}

/// Reads the archive from r and unpacks it into dir (which must exist).
pub fn unpack(r: &mut impl Read, dir: &Path) -> io::Result<Unpacked> {
    let mut done = Unpacked::default();
    let mut long_name: Option<String> = None; // from a GNU "L" or pax "x" entry, for the next one
    let mut entries = 0usize;
    let mut header = [0u8; 512];
    loop {
        if !read_block(r, &mut header)? {
            break; // ended without the two zero blocks: fine
        }
        if header.iter().all(|b| *b == 0) {
            break; // end of archive
        }
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(invalid("too many files in the archive"));
        }
        // The checksum: the header's bytes summed, with its own field as spaces.
        let want = octal(&header[148..156])?;
        let sum: u64 = header.iter().enumerate()
            .map(|(i, b)| if (148..156).contains(&i) { b' ' as u64 } else { *b as u64 }).sum();
        if sum != want {
            return Err(invalid("not a .tar file (or it's damaged)"));
        }
        let size = octal(&header[124..136])?;
        let mode = octal(&header[100..108]).unwrap_or(0o644);
        let kind = header[156];
        let mut name = text(&header[0..100]);
        if &header[257..262] == b"ustar" {
            let prefix = text(&header[345..500]);
            if !prefix.is_empty() { name = format!("{prefix}/{name}"); }
        }
        if let Some(n) = long_name.take() { name = n; }

        match kind {
            b'L' => {
                // GNU long name: the data is the next entry's name.
                long_name = Some(read_small(r, size)?.trim_end_matches('\0').to_string());
                continue;
            }
            b'x' => {
                // pax extended header: "len key=value\n" records; only path matters.
                let data = read_small(r, size)?;
                for rec in data.split('\n') {
                    if let Some((_, kv)) = rec.split_once(' ') {
                        if let Some(p) = kv.strip_prefix("path=") { long_name = Some(p.to_string()); }
                    }
                }
                continue;
            }
            b'g' => { skip(r, size)?; continue; } // global pax header: nothing we use
            _ => {}
        }

        // The archive's own top folder ("./", as tar -C dir . writes it).
        if kind == b'5' && matches!(name.trim_end_matches('/'), "" | ".") {
            skip(r, size)?;
            continue;
        }
        let rel = safe_path(&name).ok_or_else(|| invalid(format!("unsafe path in the archive: {name}")))?;
        let dest = dir.join(&rel);
        match kind {
            b'0' | 0 | b'7' => {
                done.bytes = done.bytes.saturating_add(size);
                if done.bytes > MAX_TOTAL {
                    return Err(invalid("the archive is too big"));
                }
                if let Some(parent) = dest.parent() { fs::create_dir_all(parent)?; }
                let mut f = fs::File::create(&dest)?;
                copy_exact(r, &mut f, size)?;
                pad(r, size)?;
                set_executable(&dest, mode & 0o111 != 0)?;
                done.files += 1;
            }
            b'5' => {
                fs::create_dir_all(&dest)?;
                skip(r, size)?;
            }
            b'1' | b'2' => return Err(invalid(format!("links aren't allowed in a plugin archive: {name}"))),
            _ => return Err(invalid(format!("unsupported entry in the archive: {name}"))),
        }
    }
    Ok(done)
}

/// Reads one 512-byte block; false at a clean end of input.
fn read_block(r: &mut impl Read, buf: &mut [u8; 512]) -> io::Result<bool> {
    let mut got = 0;
    while got < 512 {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            return if got == 0 { Ok(false) } else { Err(invalid("the archive ends part-way through")) };
        }
        got += n;
    }
    Ok(true)
}

fn read_small(r: &mut impl Read, size: u64) -> io::Result<String> {
    if size > 1 << 20 { return Err(invalid("a header in the archive is too big")); }
    let mut v = vec![0u8; size as usize];
    r.read_exact(&mut v)?;
    pad(r, size)?;
    Ok(String::from_utf8_lossy(&v).into_owned())
}

fn copy_exact(r: &mut impl Read, w: &mut impl Write, size: u64) -> io::Result<()> {
    let n = io::copy(&mut r.take(size), w)?;
    if n != size { return Err(invalid("the archive ends part-way through a file")); }
    Ok(())
}

fn skip(r: &mut impl Read, size: u64) -> io::Result<()> {
    copy_exact(r, &mut io::sink(), size)?;
    pad(r, size)
}

/// Entries are padded to a whole number of 512-byte blocks.
fn pad(r: &mut impl Read, size: u64) -> io::Result<()> {
    let rem = (512 - size % 512) % 512;
    copy_exact(r, &mut io::sink(), rem)
}

#[cfg(unix)]
fn set_executable(p: &Path, exec: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))
}
#[cfg(not(unix))]
fn set_executable(_: &Path, _: bool) -> io::Result<()> { Ok(()) }

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a ustar entry the way tar/Go's archive/tar do.
    fn entry(out: &mut Vec<u8>, name: &str, kind: u8, data: &[u8], mode: u32) {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(format!("{mode:07o}").as_bytes());
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|b| *b as u32).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        out.resize(out.len() + (512 - data.len() % 512) % 512, 0);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarfile-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unpacks_files_and_folders() {
        let mut a = Vec::new();
        entry(&mut a, "./", b'5', b"", 0o755);
        entry(&mut a, "plugin.json", b'0', b"{\"id\":\"x\"}", 0o644);
        entry(&mut a, "bin/", b'5', b"", 0o755);
        entry(&mut a, "bin/run", b'0', &vec![7u8; 1500], 0o755);
        // A long name, the GNU way.
        let long = format!("assets/{}.txt", "n".repeat(120));
        entry(&mut a, "././@LongLink", b'L', long.as_bytes(), 0o644);
        entry(&mut a, "assets/short", b'0', b"long one", 0o644);
        a.extend_from_slice(&[0u8; 1024]);
        let d = tmp("ok");
        let u = unpack(&mut &a[..], &d).unwrap();
        assert_eq!(u.files, 3);
        assert_eq!(fs::read(d.join("plugin.json")).unwrap(), b"{\"id\":\"x\"}");
        assert_eq!(fs::read(d.join("bin/run")).unwrap().len(), 1500);
        assert_eq!(fs::read(d.join(&long)).unwrap(), b"long one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(d.join("bin/run")).unwrap().permissions().mode() & 0o111, 0o111);
            assert_eq!(fs::metadata(d.join("plugin.json")).unwrap().permissions().mode() & 0o111, 0);
        }
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_escapes_and_links() {
        for (name, kind) in [("./", b'0'), ("../evil", b'0'), ("/etc/evil", b'0'), ("a/../../evil", b'0'), ("link", b'2'), ("C:/x", b'0')] {
            let mut a = Vec::new();
            entry(&mut a, name, kind, b"x", 0o644);
            a.extend_from_slice(&[0u8; 1024]);
            let d = tmp("bad");
            assert!(unpack(&mut &a[..], &d).is_err(), "{name} should be refused");
            assert!(!d.parent().unwrap().join("evil").exists());
            fs::remove_dir_all(&d).unwrap();
        }
    }

    #[test]
    fn refuses_garbage() {
        let d = tmp("junk");
        let junk = vec![0x41u8; 2048];
        assert!(unpack(&mut &junk[..], &d).is_err());
        // Cut short in the middle of a file.
        let mut a = Vec::new();
        entry(&mut a, "f", b'0', &vec![1u8; 2000], 0o644);
        a.truncate(900);
        assert!(unpack(&mut &a[..], &d).is_err());
        fs::remove_dir_all(&d).unwrap();
    }

    /// An archive given on the command line (TAR_FILE=…), e.g. a real
    /// plugin built with tar: it must unpack.
    #[test]
    fn unpacks_a_real_archive() {
        let Ok(path) = std::env::var("TAR_FILE") else { return };
        let d = tmp("real");
        let u = unpack(&mut fs::File::open(path).unwrap(), &d).unwrap();
        assert!(u.files > 0 && d.join("plugin.json").is_file());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn safe_paths() {
        assert_eq!(safe_path("./a/b"), Some(PathBuf::from("a/b")));
        assert_eq!(safe_path("a/./b/"), Some(PathBuf::from("a/b")));
        assert!(safe_path("").is_none() && safe_path(".").is_none() && safe_path("a\\..\\b").is_none());
    }
}
