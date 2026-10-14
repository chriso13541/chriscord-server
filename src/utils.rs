use rand::Rng;
use std::sync::atomic::{AtomicU16, Ordering};

/// The port this server listens on, for chat (TCP) and voice (UDP):
/// 7070 unless --port or CHRISCORD_PORT says otherwise (main.rs).
static PORT: AtomicU16 = AtomicU16::new(7070);

pub fn port() -> u16 { PORT.load(Ordering::Relaxed) }
pub fn set_port(p: u16) { PORT.store(p, Ordering::Relaxed) }

pub fn generate_hex(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| format!("{:x}", rng.gen::<u8>() & 0xf))
        .collect()
}
