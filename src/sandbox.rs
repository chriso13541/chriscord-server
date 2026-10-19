//! A sandbox for plugins (plugins.rs), with Linux's Landlock: what a plugin
//! can do on this machine, as opposed to on the server (its permissions
//! and channels, which the server already limits).
//!
//! In the sandbox a plugin can:
//!   - read and write its own data folder (plugin-data/<id>), which is also
//!     its home and its temporary folder;
//!   - read and run its own code (plugins/<id>) and the system's programs
//!     and libraries (/usr, /etc, /opt…: ffmpeg, Deno, certificates, DNS);
//!   - connect to this server, and look up names (DNS); and, if it asked for
//!     the internet and the owner allowed it, to web sites (ports 80, 443).
//! and it can't:
//!   - read or change anything else: not the server's folder (its database,
//!     settings and keys), not other plugins, not anyone's home folder;
//!   - touch other processes: no debugging or reading the server's memory
//!     or environment, and (Linux 6.12+) no signals to anything outside it;
//!   - (Linux 6.7+) open TCP ports, or connect to other TCP ports.
//!
//! What Landlock doesn't cover: UDP (calls need it anyway), and connecting
//! to local UNIX sockets the server's user can reach — so don't run the
//! server as a user with access to, say, Docker's socket.
//!
//! It needs Linux 5.13+ with Landlock turned on (most distributions since
//! 2022). Where it isn't available the plugin still runs, and the admin
//! panel says it isn't sandboxed and why. Only the standard library is
//! used, so it's tested on its own:
//! `rustc --edition 2021 --test src/sandbox.rs && ./sandbox`.

use std::io;
use std::path::{Path, PathBuf};

/// Whether plugins are sandboxed here, and if not, why (for the panel).
#[derive(Clone, Debug)]
pub struct Support {
    pub sandboxed: bool,
    /// Whether TCP ports can be limited too (Landlock 4+, Linux 6.7+).
    pub net: bool,
    /// Landlock's version on this machine (0: none).
    pub abi: i32,
    pub note: String,
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;
    pub struct Sandbox;
    pub fn support() -> Support {
        Support { sandboxed: false, net: false, abi: 0, note: "Plugins are only sandboxed on Linux".into() }
    }
    pub fn prepare(_: &Path, _: &Path, _: &Path, _: Option<&[u16]>) -> Result<Sandbox, String> { Err(support().note) }
    impl Sandbox {
        pub fn raw(&self) -> i32 { -1 }
    }
    /// # Safety
    /// Called in the child between fork and exec.
    pub unsafe fn enter(_: i32) -> io::Result<()> { Ok(()) }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_long};
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn syscall(num: c_long, ...) -> c_long;
        fn prctl(option: c_int, ...) -> c_int;
        fn open(path: *const c_char, flags: c_int, ...) -> c_int;
        fn close(fd: c_int) -> c_int;
    }

    // The same numbers on every architecture.
    const SYS_CREATE_RULESET: c_long = 444;
    const SYS_ADD_RULE: c_long = 445;
    const SYS_RESTRICT_SELF: c_long = 446;
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: c_int = 1;
    const RULE_NET_PORT: c_int = 2;
    // TCP (LANDLOCK_ACCESS_NET_*, ABI 4).
    const NET_BIND_TCP: u64 = 1 << 0;
    const NET_CONNECT_TCP: u64 = 1 << 1;
    const PR_SET_NO_NEW_PRIVS: c_int = 38;
    const O_PATH: c_int = 0o10000000;
    const O_CLOEXEC: c_int = 0o2000000;

    // What can be done to files (LANDLOCK_ACCESS_FS_*).
    const EXECUTE: u64 = 1 << 0;
    const WRITE_FILE: u64 = 1 << 1;
    const READ_FILE: u64 = 1 << 2;
    const READ_DIR: u64 = 1 << 3;
    const TRUNCATE: u64 = 1 << 14; // ABI 3
    const IOCTL_DEV: u64 = 1 << 15; // ABI 5
    // Signals and abstract sockets stay inside the sandbox (ABI 6).
    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    const SCOPE_SIGNAL: u64 = 1 << 1;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
        scoped: u64,
    }

    #[repr(C)]
    struct NetPortAttr {
        allowed_access: u64,
        port: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// Landlock's version here, or why there's none.
    fn abi() -> Result<i32, String> {
        let v = unsafe { syscall(SYS_CREATE_RULESET, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
        if v >= 1 { return Ok(v as i32); }
        Err(match io::Error::last_os_error().raw_os_error() {
            Some(38) => "this Linux kernel is older than 5.13, or a container blocks it (Landlock isn't available)".into(),
            Some(95) => "Landlock is turned off in this Linux kernel (add landlock to the lsm= boot option)".into(),
            _ => format!("Landlock isn't available: {}", io::Error::last_os_error()),
        })
    }

    /// Everything this version can restrict, for files.
    fn handled_fs(abi: i32) -> u64 {
        let mut all = (1u64 << 13) - 1; // ABI 1: execute … make_sym
        if abi >= 2 { all |= 1 << 13; } // refer (moving files between folders)
        if abi >= 3 { all |= TRUNCATE; }
        if abi >= 5 { all |= IOCTL_DEV; }
        all
    }

    pub fn support() -> Support {
        match abi() {
            Ok(v) if v >= 4 => Support { sandboxed: true, net: true, abi: v, note: format!("Sandboxed with Landlock (version {v})") },
            Ok(v) => Support { sandboxed: true, net: false, abi: v,
                note: format!("Sandboxed with Landlock (version {v}); its network isn't limited (that needs Linux 6.7 or newer)") },
            Err(why) => Support { sandboxed: false, net: false, abi: 0, note: why },
        }
    }

    /// A ruleset made for one plugin, ready for its program to enter.
    pub struct Sandbox {
        fd: c_int,
    }

    impl Drop for Sandbox {
        fn drop(&mut self) { unsafe { close(self.fd); } }
    }

    impl Sandbox {
        pub fn raw(&self) -> i32 { self.fd }

        fn allow(&self, path: &Path, access: u64) -> Result<(), String> {
            let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| "bad path".to_string())?;
            let fd = unsafe { open(c.as_ptr(), O_PATH | O_CLOEXEC) };
            if fd < 0 {
                return Ok(()); // not on this machine: nothing to allow
            }
            // Files can't be given folder rights.
            let access = if path.is_dir() { access } else { access & (EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV) };
            let attr = PathBeneathAttr { allowed_access: access, parent_fd: fd };
            let r = unsafe { syscall(SYS_ADD_RULE, self.fd as c_long, RULE_PATH_BENEATH, &attr as *const PathBeneathAttr, 0u32) };
            let e = io::Error::last_os_error();
            unsafe { close(fd); }
            if r != 0 { return Err(format!("couldn't allow {}: {e}", path.display())); }
            Ok(())
        }

        fn allow_port(&self, port: u16) -> Result<(), String> {
            let attr = NetPortAttr { allowed_access: NET_CONNECT_TCP, port: port as u64 };
            let r = unsafe { syscall(SYS_ADD_RULE, self.fd as c_long, RULE_NET_PORT, &attr as *const NetPortAttr, 0u32) };
            if r != 0 { return Err(format!("couldn't allow port {port}: {}", io::Error::last_os_error())); }
            Ok(())
        }

        /// Allows dir, except avoid (and what's in it) when it's inside: then
        /// each of dir's entries is allowed instead, apart from the way to avoid.
        fn allow_except(&self, dir: &Path, avoid: &Path, access: u64) -> Result<(), String> {
            if !avoid.starts_with(dir) {
                return self.allow(dir, access);
            }
            if avoid == dir { return Ok(()); }
            let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
            for e in rd.flatten() {
                let p = e.path();
                if avoid.starts_with(&p) {
                    // Down the way to it (a real folder, not a link).
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        self.allow_except(&p, avoid, access)?;
                    }
                } else {
                    self.allow(&p, access)?;
                }
            }
            Ok(())
        }
    }

    /// The system folders a program may read and run things from.
    const SYSTEM: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt",
        "/nix", "/snap", "/var/lib/snapd", "/proc", "/sys",
        // Where /etc/resolv.conf usually points (DNS).
        "/run/systemd/resolve", "/run/resolvconf", "/run/NetworkManager"];
    /// Devices it may use.
    const DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"];

    /// A ruleset for a plugin with its code in code and its data in data;
    /// server is the server's own folder, which it mustn't see (other than
    /// code and data, which may be inside it). tcp: the TCP ports it may
    /// connect to (None: any); it may never open one.
    pub fn prepare(code: &Path, data: &Path, server: &Path, tcp: Option<&[u16]>) -> Result<Sandbox, String> {
        let abi = abi()?;
        let fs = handled_fs(abi);
        let limit_net = abi >= 4 && tcp.is_some();
        let attr = RulesetAttr {
            handled_access_fs: fs,
            handled_access_net: if limit_net { NET_BIND_TCP | NET_CONNECT_TCP } else { 0 },
            scoped: if abi >= 6 { SCOPE_SIGNAL | SCOPE_ABSTRACT_UNIX_SOCKET } else { 0 },
        };
        let size = if abi >= 6 { 24usize } else if abi >= 4 { 16 } else { 8 };
        let fd = unsafe { syscall(SYS_CREATE_RULESET, &attr as *const RulesetAttr, size, 0u32) };
        if fd < 0 { return Err(format!("Landlock refused the ruleset: {}", io::Error::last_os_error())); }
        let sb = Sandbox { fd: fd as c_int };

        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let server = canon(server);
        let read_run = READ_FILE | READ_DIR | EXECUTE;
        for d in SYSTEM {
            sb.allow_except(Path::new(d), &server, read_run)?;
        }
        for d in DEVICES {
            sb.allow(Path::new(d), READ_FILE | WRITE_FILE | (fs & IOCTL_DEV))?;
        }
        sb.allow(Path::new("/dev/shm"), fs)?;
        sb.allow(&canon(code), read_run)?;
        sb.allow(&canon(data), fs)?;
        if limit_net {
            for p in tcp.unwrap_or(&[]) { sb.allow_port(*p)?; }
        }
        Ok(sb)
    }

    /// Enters the sandbox: called in the plugin's process just before its
    /// program starts (between fork and exec, so only system calls here).
    ///
    /// # Safety
    /// Only between fork and exec, with fd from Sandbox::raw.
    pub unsafe fn enter(fd: i32) -> io::Result<()> {
        if prctl(PR_SET_NO_NEW_PRIVS, 1 as std::os::raw::c_ulong, 0 as std::os::raw::c_ulong, 0 as std::os::raw::c_ulong, 0 as std::os::raw::c_ulong) != 0 {
            return Err(io::Error::last_os_error());
        }
        if syscall(SYS_RESTRICT_SELF, fd as c_long, 0u32) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub use imp::{enter, prepare, support, Sandbox};

/// Where a sandboxed plugin keeps its temporary files (inside its data).
pub fn temp_dir(data: &Path) -> PathBuf { data.join(".tmp") }

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    fn run_in(sb: &Sandbox, cmd: &str) -> std::process::Output {
        let fd = sb.raw();
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(cmd).env("HOME", "/nonexistent");
        unsafe { c.pre_exec(move || enter(fd)); }
        c.output().unwrap()
    }

    #[test]
    fn keeps_a_plugin_in_its_folders() {
        if !support().sandboxed { eprintln!("no Landlock here: {}", support().note); return; }
        let base = std::env::temp_dir().join(format!("sandbox-test-{}", std::process::id()));
        let server = base.join("server");
        let code = server.join("plugins/music");
        let data = server.join("plugin-data/music");
        let other = server.join("plugin-data/other");
        for d in [&code, &data, &other] { std::fs::create_dir_all(d).unwrap(); }
        std::fs::write(server.join("chriscord.db"), "secret").unwrap();
        std::fs::write(other.join("x"), "theirs").unwrap();
        std::fs::write(code.join("run.sh"), "echo hello from the plugin").unwrap();
        let sb = prepare(&code, &data, &server, None).unwrap();

        let ok = |o: std::process::Output| { assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr)); String::from_utf8_lossy(&o.stdout).to_string() };
        assert_eq!(ok(run_in(&sb, &format!("sh {}/run.sh", code.display()))).trim(), "hello from the plugin");
        ok(run_in(&sb, &format!("echo kept > {0}/f && cat {0}/f && mkdir -p {0}/sub && rm {0}/f", data.display())));
        ok(run_in(&sb, "cat /etc/hostname /dev/null >/dev/null; ls /usr/bin >/dev/null"));
        // Not the server's folder, other plugins, its own code (to change), or /tmp.
        for bad in [
            format!("cat {}/chriscord.db", server.display()),
            format!("ls {}", server.display()),
            format!("cat {}/x", other.display()),
            format!("echo x > {}/run.sh", code.display()),
            format!("echo x > {}/evil", std::env::temp_dir().display()),
            "cat /proc/1/environ".to_string(),
        ] {
            assert!(!run_in(&sb, &bad).status.success(), "should be refused: {bad}");
        }
        // And this process (the "server") is outside, unaffected.
        assert_eq!(std::fs::read_to_string(server.join("chriscord.db")).unwrap(), "secret");
        drop(sb);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn only_the_tcp_ports_given() {
        let s = support();
        if !s.net { eprintln!("Landlock here can't limit the network"); return; }
        use std::net::TcpListener;
        let ok = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let (okp, otherp) = (ok.local_addr().unwrap().port(), other.local_addr().unwrap().port());
        let base = std::env::temp_dir().join(format!("sandbox-net-{}", std::process::id()));
        let d = base.join("d");
        std::fs::create_dir_all(&d).unwrap();
        let sb = prepare(&d, &d, &base.join("server"), Some(&[okp])).unwrap();
        let connect = |p: u16| run_in(&sb, &format!("exec bash -c 'exec 3<>/dev/tcp/127.0.0.1/{p}'")).status.success();
        assert!(connect(okp), "the allowed port");
        assert!(!connect(otherp), "another port");
        // And it can't open a port of its own.
        let o = run_in(&sb, "exec python3 -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0)); s.listen()'");
        assert!(!o.status.success(), "listening should be refused");
        // Without a limit, both work.
        let open = prepare(&d, &d, &base.join("server"), None).unwrap();
        assert!(run_in(&open, &format!("exec bash -c 'exec 3<>/dev/tcp/127.0.0.1/{otherp}'")).status.success());
        drop((sb, open));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn signals_stay_inside() {
        let s = support();
        if !s.sandboxed || s.abi < 6 { return; }
        let base = std::env::temp_dir().join(format!("sandbox-sig-{}", std::process::id()));
        let d = base.join("d");
        std::fs::create_dir_all(&d).unwrap();
        let sb = prepare(&d, &d, &base.join("server"), None).unwrap();
        // Signal 0 to this (outside) process: refused from inside.
        let o = run_in(&sb, &format!("kill -0 {}", std::process::id()));
        assert!(!o.status.success());
        drop(sb);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
