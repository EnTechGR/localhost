//! Session management via HTTP cookies.
//!
//! Provides an in-memory session store keyed by a random session ID and
//! the cookie helpers needed to persist that ID on the client side.
//!
//! # Module layout
//!
//! - [`store`]  — `SessionStore`, `SessionData`, `SessionId`
//! - [`cookie`] — `parse_cookies`, `set_cookie_header`, `CookieOptions`
//!
//! # Integration (handled by the dispatcher, not this module)
//!
//! **On each request:**
//! 1. Read the `Cookie` header → [`parse_cookies`].
//! 2. Extract the session ID → [`extract_session_id`].
//! 3. Look up the session → [`SessionStore::get_mut`] (creates a new one if
//!    the ID is missing or unknown).
//!
//! **On each response:**
//! - If a new session was created, add a `Set-Cookie` header via
//!   [`set_cookie_header`].
//!
//! **Per event-loop tick:**
//! - Call [`SessionStore::purge_expired`] to evict stale sessions.
pub mod cookie;
pub mod store;

pub use cookie::{
    extract_session_id, parse_cookies, set_cookie_header, CookieOptions, SESSION_COOKIE,
};
pub use store::{SessionData, SessionId, SessionStore};