//! Telegram OSINT layer: public channel monitoring (keyed, Phase-1 deferred).
//!
//! Config plumbing is complete: `TELEGRAM_API_ID` / `TELEGRAM_API_HASH`
//! (from https://my.telegram.org) plus the `TELEGRAM_CHANNELS` allowlist.
//! When the credentials are unset, `/api/telegram` answers 503 by design,
//! mirroring OSIRIS's disabled-backend convention.
//!
//! A full MTProto client is deliberately OUT OF SCOPE for Phase 1, so the
//! route also answers 503 when credentials ARE set. Rationale and intended
//! approach:
//!
//! - MTProto is a stateful binary protocol, not REST: it needs a persistent
//!   authorized *session*, not just an api_id/api_hash pair. First-time
//!   authorization is interactive (phone number → SMS/code login), which
//!   does not fit an env-key-only deployment. The workable pattern is a
//!   long-lived session file/string created once out-of-band, then reused
//!   by the server.
//! - Candidate Rust implementations: `grammers` (pure-Rust MTProto) or
//!   `tdlib` bindings. Neither is in `Cargo.toml` today (which Phase 1 may
//!   not touch), so no client is wired.
//! - Intended Phase-2 design: on startup, load the session, `channels.join`
//!   / resolve each allowlisted public channel, poll `messages.getHistory`
//!   per channel on the feed TTL, and normalize to
//!   `{channel, message_id, date, text, views, forwards, url}`. Respect
//!   Telegram flood-wait limits; never attempt to join private channels.
//! - Privacy note: only *public* channels on the operator-configured
//!   allowlist are ever read; the `?channel=` parameter is validated
//!   against that allowlist when it is non-empty.
//!
//! UNVERIFIED/deferred-pending-creds: no Telegram credentials or session
//! exist in this environment; nothing here has touched the Telegram API.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
};

/// Cache TTL (applies once the MTProto client exists).
pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "telegram";

/// Intended normalization target for a future MTProto client: one message.
#[derive(Debug, Serialize)]
pub struct TelegramMessage {
    pub channel: String,
    pub message_id: i64,
    pub date: String,
    pub text: String,
    pub views: Option<i64>,
    pub url: String,
}

/// Intended normalization target for a future MTProto client.
#[derive(Debug, Serialize)]
pub struct TelegramResponse {
    pub count: usize,
    pub generated_at: String,
    pub channel: String,
    pub messages: Vec<TelegramMessage>,
}

/// Resolve the effective channel: explicit `?channel=` wins, otherwise the
/// first configured allowlist entry. Errors when neither exists.
fn resolve_channel(query: Option<&str>, config: &Config) -> Result<String, ApiError> {
    if let Some(c) = query.map(str::trim).filter(|c| !c.is_empty()) {
        let c = c.trim_start_matches('@').to_string();
        if !config.telegram_channels.is_empty() && !config.telegram_channels.iter().any(|a| a == &c)
        {
            return Err(ApiError::bad_request(
                "channel not allowlisted",
                "the requested channel is not in TELEGRAM_CHANNELS",
            ));
        }
        return Ok(c);
    }
    config.telegram_channels.first().cloned().ok_or_else(|| {
        ApiError::bad_request(
            "missing channel",
            "query param channel is required, e.g. ?channel=example (or set TELEGRAM_CHANNELS)",
        )
    })
}

/// Short detail string for the health endpoint.
pub async fn probe_detail(_client: &reqwest::Client, config: &Config) -> Result<String, FeedError> {
    match (&config.telegram_api_id, &config.telegram_api_hash) {
        (None, _) | (_, None) => Err(FeedError::new(
            FEED,
            "TELEGRAM_API_ID/TELEGRAM_API_HASH unset — feed disabled",
        )),
        (Some(_), Some(_)) => Err(FeedError::new(
            FEED,
            "MTProto client deferred to a later phase (see module docs)",
        )),
    }
}

// ---------------------------------------------------------------------------
// GET /api/telegram?channel=
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TelegramQuery {
    pub channel: Option<String>,
}

pub async fn handler(
    State(state): State<AppState>,
    Query(q): Query<TelegramQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let _channel = resolve_channel(q.channel.as_deref(), &state.config)?;
    // Always 503 in Phase 1 (see module docs); the turbofish pins the
    // opaque `impl IntoResponse` to `Response` for type inference.
    Err::<Response, ApiError>(
        match (
            &state.config.telegram_api_id,
            &state.config.telegram_api_hash,
        ) {
            (None, _) | (_, None) => ApiError::service_unavailable(
                "telegram feed disabled",
                "set TELEGRAM_API_ID and TELEGRAM_API_HASH from https://my.telegram.org to enable /api/telegram (then configure TELEGRAM_CHANNELS with the public channels to monitor; MTProto client deferred to a later phase — see src/feeds/telegram.rs)",
            ),
            (Some(_), Some(_)) => ApiError::service_unavailable(
                "telegram feed not implemented",
                "Telegram API credentials are configured but the MTProto client is deferred to a later phase (a persistent authorized session is required; api_id/api_hash alone are not sufficient). Intended implementation documented in src/feeds/telegram.rs",
            ),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_channel_wins() {
        let config = Config::test_default();
        assert_eq!(resolve_channel(Some("alpha"), &config).unwrap(), "alpha");
    }

    #[test]
    fn at_prefix_stripped() {
        let config = Config::test_default();
        assert_eq!(resolve_channel(Some("@alpha"), &config).unwrap(), "alpha");
    }

    #[test]
    fn missing_channel_without_allowlist_is_400() {
        let config = Config::test_default();
        assert!(resolve_channel(None, &config).is_err());
    }

    #[test]
    fn non_allowlisted_channel_rejected() {
        let mut config = Config::test_default();
        config.telegram_channels = vec!["alpha".to_string()];
        assert!(resolve_channel(Some("beta"), &config).is_err());
        assert_eq!(resolve_channel(Some("alpha"), &config).unwrap(), "alpha");
    }

    #[test]
    fn allowlist_default_used_when_no_param() {
        let mut config = Config::test_default();
        config.telegram_channels = vec!["alpha".to_string(), "beta".to_string()];
        assert_eq!(resolve_channel(None, &config).unwrap(), "alpha");
    }
}
