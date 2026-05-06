pub fn hash_str(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}
