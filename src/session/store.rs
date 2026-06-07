//! In-memory session store.
//!
//! Each session is a bag of string key-value pairs with creation and
//! last-accessed timestamps. The store itself is a `HashMap` keyed by a
//! cryptographically random 32-character hex string (16 bytes from
//! `/dev/urandom`). Because the server is single-threaded, no locking is
//! needed.
//!
//! The dispatcher calls [`SessionStore::purge_expired`] once per event-loop
//! tick (inside the timeout sweep) to evict stale sessions.
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Opaque session identifier — a 32-character lowercase hex string.
pub type SessionId = String;

// ---------------------------------------------------------------------------
// SessionData
// ---------------------------------------------------------------------------

/// Per-session payload: application-defined key-value pairs plus timestamps.
#[derive(Debug)]
pub struct SessionData {
    /// Arbitrary key-value pairs stored by the application or handlers.
    pub values: HashMap<String, String>,
    /// When this session was first created.
    pub created: Instant,
    /// When this session was last read or written.
    pub last_accessed: Instant,
}

impl SessionData {
    fn new() -> Self {
        let now = Instant::now();
        SessionData {
            values: HashMap::new(),
            created: now,
            last_accessed: now,
        }
    }

    fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// SessionStore
// ---------------------------------------------------------------------------

/// Single-threaded in-memory session store.
pub struct SessionStore {
    sessions: HashMap<SessionId, SessionData>,
}

impl SessionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        SessionStore {
            sessions: HashMap::new(),
        }
    }

    /// Create a fresh session and return its unique ID.
    ///
    /// The ID is 16 bytes read from `/dev/urandom`, hex-encoded to 32 chars.
    pub fn create(&mut self) -> SessionId {
        let id = generate_id();
        self.sessions.insert(id.clone(), SessionData::new());
        id
    }

    /// Look up a session by ID (immutable). Does **not** update timestamps.
    pub fn get(&self, id: &str) -> Option<&SessionData> {
        self.sessions.get(id)
    }

    /// Look up a session by ID (mutable). Updates `last_accessed`.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut SessionData> {
        self.sessions.get_mut(id).map(|s| {
            s.touch();
            s
        })
    }

    /// Remove a session.
    pub fn destroy(&mut self, id: &str) {
        self.sessions.remove(id);
    }

    /// Evict every session whose `last_accessed` is older than `max_age`.
    ///
    /// Called once per event-loop tick from the timeout sweep.
    pub fn purge_expired(&mut self, max_age: Duration) {
        self.sessions
            .retain(|_, data| data.last_accessed.elapsed() < max_age);
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether the store contains no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Random ID generation
// ---------------------------------------------------------------------------

/// Generate a 32-char hex session ID from 16 bytes of `/dev/urandom`.
fn generate_id() -> SessionId {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(&mut buf).is_err() {
            fallback_entropy(&mut buf);
        }
    } else {
        fallback_entropy(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Last-resort entropy when `/dev/urandom` is unavailable (should never
/// happen on Linux, but the server must not panic).
fn fallback_entropy(buf: &mut [u8; 16]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    buf[..8].copy_from_slice(&now.to_le_bytes());
    let ptr = buf.as_ptr() as u64;
    buf[8..].copy_from_slice(&ptr.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn create_returns_32_char_hex() {
        let mut store = SessionStore::new();
        let id = store.create();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn create_produces_unique_ids() {
        let mut store = SessionStore::new();
        let a = store.create();
        let b = store.create();
        assert_ne!(a, b);
    }

    #[test]
    fn get_finds_created_session() {
        let mut store = SessionStore::new();
        let id = store.create();
        assert!(store.get(&id).is_some());
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let store = SessionStore::new();
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn get_mut_touches_last_accessed() {
        let mut store = SessionStore::new();
        let id = store.create();
        let t0 = store.get(&id).unwrap().last_accessed;
        thread::sleep(Duration::from_millis(5));
        let _ = store.get_mut(&id);
        let t1 = store.get(&id).unwrap().last_accessed;
        assert!(t1 > t0);
    }

    #[test]
    fn get_mut_allows_value_insertion() {
        let mut store = SessionStore::new();
        let id = store.create();
        store
            .get_mut(&id)
            .unwrap()
            .values
            .insert("user".into(), "alice".into());
        assert_eq!(
            store.get(&id).unwrap().values.get("user").map(String::as_str),
            Some("alice")
        );
    }

    #[test]
    fn destroy_removes_session() {
        let mut store = SessionStore::new();
        let id = store.create();
        assert_eq!(store.len(), 1);
        store.destroy(&id);
        assert_eq!(store.len(), 0);
        assert!(store.get(&id).is_none());
    }

    #[test]
    fn purge_expired_removes_old_sessions() {
        let mut store = SessionStore::new();
        let old = store.create();
        thread::sleep(Duration::from_millis(20));
        let fresh = store.create();

        // Purge anything older than 10 ms.
        store.purge_expired(Duration::from_millis(10));

        assert!(store.get(&old).is_none(), "old session should be purged");
        assert!(store.get(&fresh).is_some(), "fresh session should survive");
    }

    #[test]
    fn len_and_is_empty() {
        let mut store = SessionStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        store.create();
        assert!(!store.is_empty());
        assert_eq!(store.len(), 1);
    }
}