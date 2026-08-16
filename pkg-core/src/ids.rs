use sha2::{Digest, Sha256};

/// New random uid for sync identity (spec §4.1/§4.3: TEXT uid alongside INTEGER pk).
pub fn new_uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Content hash used for idempotent re-ingest (spec §5.3).
pub fn content_hash(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Current UTC time in SQLite's datetime format.
pub fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Lowercased canonical form for entity names.
pub fn canonicalize(name: &str) -> String {
    name.trim().to_lowercase()
}
