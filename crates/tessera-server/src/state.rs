//! Shared server state: the engine, and the per-token session registry Task 11's report flags as
//! the server's (not the engine's) responsibility to own.

use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use tessera_engine::{Engine, Session};
use tessera_wire::HandleTable;

use crate::error::ApiError;

/// One authorised session: the engine's [`Session`] plus its own per-session handle table (I10) —
/// held alongside, not inside, `Session` (Task 11's report flags this as the intended seam, since
/// `tessera-wire` must not depend on `tessera-engine`'s `EntityId`).
pub struct SessionEntry {
    pub session: Session,
    pub handles: Mutex<HandleTable>,
}

/// Every live session, indexed both by bearer token (the viewer plane's lookup) and by
/// `token_id` (`/session/revoke`'s request shape, R5).
#[derive(Default)]
pub struct SessionRegistry {
    by_token: FxHashMap<String, std::sync::Arc<SessionEntry>>,
    token_id_to_token: FxHashMap<u64, String>,
}

impl SessionRegistry {
    pub fn insert(&mut self, session: Session) -> std::sync::Arc<SessionEntry> {
        let token = session.token.clone();
        let token_id = session.token_id;
        let entry = std::sync::Arc::new(SessionEntry {
            session,
            handles: Mutex::new(HandleTable::new()),
        });
        self.by_token
            .insert(token.clone(), std::sync::Arc::clone(&entry));
        self.token_id_to_token.insert(token_id, token);
        entry
    }

    pub fn get(&self, token: &str) -> Option<std::sync::Arc<SessionEntry>> {
        self.by_token.get(token).cloned()
    }

    /// Revoke by `token_id` (R5): a token id this registry never minted, or already revoked, is
    /// simply a no-op — `/session/revoke` is 204 either way (revoking twice is not an error).
    pub fn revoke(&mut self, token_id: u64) {
        if let Some(token) = self.token_id_to_token.remove(&token_id) {
            self.by_token.remove(&token);
        }
    }
}

/// Process-wide server state, shared (behind `Arc`) across every axum handler on every plane.
pub struct AppState {
    pub engine: Engine,
    pub sessions: Mutex<SessionRegistry>,
    pub max_k: usize,
    /// Parsed and stored (design §7.5/§2.3's startup rule); not consumed by any Phase 1 handler.
    #[allow(dead_code)]
    pub min_visible_members: u64,
    pub session_credential: String,
    pub operator_credential: String,
}

impl AppState {
    /// Bearer-token lookup for the viewer plane: an unrecognised token is `bad-credential` (401);
    /// a recognised-but-expired one is `expired-token` (403) — the engine itself never checks
    /// `expires_at` (Task 11's report flags this as a server obligation this method exists to
    /// discharge).
    pub fn authenticated_session(
        &self,
        token: &str,
    ) -> Result<std::sync::Arc<SessionEntry>, ApiError> {
        let entry = self
            .sessions
            .lock()
            .get(token)
            .ok_or(ApiError::BadCredential)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs();
        if now >= entry.session.expires_at {
            return Err(ApiError::ExpiredToken);
        }
        Ok(entry)
    }

    /// Constant-time-ish (string equality; Phase 1 does not harden against timing side channels —
    /// out of this phase's scope) bearer check for the session/control planes' shared-secret
    /// credentials.
    pub fn check_bearer(&self, presented: Option<&str>, expected: &str) -> Result<(), ApiError> {
        match presented {
            Some(token) if token == expected => Ok(()),
            _ => Err(ApiError::BadCredential),
        }
    }
}
