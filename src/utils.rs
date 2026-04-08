use rand::Rng;

pub fn generate_hex(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| format!("{:x}", rng.gen::<u8>() & 0xf))
        .collect()
}
