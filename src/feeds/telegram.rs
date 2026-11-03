//! Telegram OSINT layer: public channel monitoring (keyless).
//!
//! Upstream: `https://t.me/s/<channel>` — Telegram's public channel preview
//! pages, readable with no login. This is the standard OSINT approach for
//! public channels and replaces the earlier deferred MTProto plan (which
//! needed an interactive authorized session — see git history for the
//! rationale that was punted).
//!
//! Message markup (verified 2026-11-01 against `t.me/s/telegram`):
//! - `div.tgme_widget_message[data-post="<channel>/<id>"]` — one per message
//!   (service messages use `tgme_widget_message_service` and are excluded)
//! - `.tgme_widget_message_text` — body; inner HTML, `<br>` → newline
//! - `a.tgme_widget_message_date > time[datetime]` — ISO 8601, `+00:00`
//! - `span.tgme_widget_message_views` — `"1.64M"` / `"12.3K"` / `"512"`
//! - `span.tgme_widget_message_forwards` — same shape, often absent
//!
//! Fragility notes: this is screen-scraping, not an API. Telegram changes
//! markup without notice; unknown structure degrades to an empty message
//! list with a logged warning, never a panic. Selectors are intentionally
//! narrow so a redesign fails closed to "no messages" rather than
//! mis-parsed data. The fixture at
//! `benches/fixtures/telegram_channel.html` pins the known-good shape.
//!
//! Responsible use: one upstream request per channel per 300s TTL through
//! the shared cache; a browser User-Agent (the preview is only served to
//! browser-like clients); no concurrent hammering — `t.me/s` is a courtesy
//! surface, not a bulk API. Only *public* channels are ever read — never
//! any auth, never private channels/groups. The `TELEGRAM_CHANNELS`
//! allowlist still gates `?channel=` when non-empty, exactly as before.

use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{HeaderName, StatusCode, header},
    response::{IntoResponse, Response},
};
use scraper::{ElementRef, Html, Selector, node::Node};
use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    error::{ApiError, FeedError},
    routes::AppState,
    time::now_iso8601,
};

/// Cache TTL: 5 minutes. One request per channel per TTL keeps us far under
/// anything resembling abusive against the preview pages.
pub const TTL: Duration = Duration::from_secs(300);
pub const FEED: &str = "telegram";

/// Curated default channel list (`data/telegram_channels.json`), used when
/// `TELEGRAM_CHANNELS` is unset. Every entry was verified live via t.me.
static DEFAULT_CHANNELS_JSON: &str = include_str!("../../data/telegram_channels.json");

/// Names of the curated default channels (data/telegram_channels.json),
/// regardless of any TELEGRAM_CHANNELS env override. Used to identify
/// news-outlet authors for exclusion from threat-actor views.
pub fn curated_channel_names() -> Vec<String> {
    let v: serde_json::Value =
        serde_json::from_str(DEFAULT_CHANNELS_JSON).expect("data/telegram_channels.json parses");
    v.get("channels")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The effective scan set: the `TELEGRAM_CHANNELS` env allowlist when set,
/// otherwise the curated defaults. Never guesses — all defaults verified.
pub fn effective_channels(config: &Config) -> Vec<String> {
    if !config.telegram_channels.is_empty() {
        return config.telegram_channels.clone();
    }
    curated_channel_names()
}

/// Browser UA: `t.me/s` serves the full message preview to browser-like
/// clients; the default reqwest UA gets a reduced page.
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// One normalized channel message.
#[derive(Debug, Serialize, Deserialize)]
pub struct TelegramMessage {
    pub channel: String,
    pub message_id: i64,
    pub date: String,
    pub text: String,
    pub views: Option<i64>,
    pub forwards: Option<i64>,
    pub url: String,
}

/// Normalized channel snapshot served by `/api/telegram`.
#[derive(Debug, Serialize, Deserialize)]
pub struct TelegramResponse {
    pub count: usize,
    pub generated_at: String,
    pub channel: String,
    pub messages: Vec<TelegramMessage>,
}

/// Telegram public usernames: 5–32 chars of `[A-Za-z0-9_]`.
fn valid_channel_name(c: &str) -> bool {
    let len = c.len();
    (5..=32).contains(&len) && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Resolve the effective channel: explicit `?channel=` wins, otherwise the
/// first entry of the effective channel list (TELEGRAM_CHANNELS env, else
/// the curated defaults). Errors when neither exists, when the name is not
/// allowlisted, or when the name is not a valid public-channel username
/// (400 in all cases).
fn resolve_channel(query: Option<&str>, config: &Config) -> Result<String, ApiError> {
    let effective = effective_channels(config);
    if let Some(c) = query.map(str::trim).filter(|c| !c.is_empty()) {
        let c = c.trim_start_matches('@').to_string();
        if !valid_channel_name(&c) {
            return Err(ApiError::bad_request(
                "invalid channel name",
                "channel must be 5-32 chars of letters, digits, underscore",
            ));
        }
        if !config.telegram_channels.is_empty() && !config.telegram_channels.iter().any(|a| a == &c)
        {
            return Err(ApiError::bad_request(
                "channel not allowlisted",
                "the requested channel is not in TELEGRAM_CHANNELS",
            ));
        }
        return Ok(c);
    }
    effective.first().cloned().ok_or_else(|| {
        ApiError::bad_request(
            "missing channel",
            "query param channel is required, e.g. ?channel=telegram (or set TELEGRAM_CHANNELS)",
        )
    })
}

/// `"1.64M"` → 1_640_000; `"12.3K"` → 12_300; `"512"` → 512.
/// Returns `None` on unparseable input rather than failing the batch.
pub fn parse_views(s: &str) -> Option<i64> {
    let s = s.trim().replace([' ', '\u{a0}'], "");
    let (num, mult) = if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1_000_000.0)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1_000.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v: f64 = num.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * mult).round() as i64)
}

/// Extract readable text from a message-body element: `<br>` becomes a
/// newline, other tags are dropped, each line trimmed, runs of blank lines
/// collapsed to one.
fn message_text(el: ElementRef) -> String {
    fn walk(el: ElementRef, out: &mut String) {
        for node in el.children() {
            match node.value() {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) if e.name() == "br" => out.push('\n'),
                Node::Element(_) => {
                    if let Some(child) = ElementRef::wrap(node) {
                        walk(child, out);
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = String::new();
    walk(el, &mut out);
    let mut cleaned: Vec<&str> = Vec::new();
    let mut prev_empty = true; // drop leading blank lines
    for line in out.lines().map(str::trim) {
        let empty = line.is_empty();
        if empty && prev_empty {
            continue;
        }
        cleaned.push(line);
        prev_empty = empty;
    }
    while cleaned.last().is_some_and(|l| l.is_empty()) {
        cleaned.pop();
    }
    cleaned.join("\n")
}

/// `2026-05-14T16:08:31+00:00` → `2026-05-14T16:08:31Z`; anything else is
/// passed through unchanged.
fn normalize_datetime(s: &str) -> String {
    s.strip_suffix("+00:00")
        .map(|t| format!("{t}Z"))
        .unwrap_or_else(|| s.to_string())
}

/// First descendant matching `selector` inside `el`, if any.
fn first<'a>(el: ElementRef<'a>, sel: &Selector) -> Option<ElementRef<'a>> {
    el.select(sel).next()
}

/// Parse a `t.me/s/<channel>` preview page into normalized messages.
/// Pure: no networking. Unknown structure → empty vec (the caller logs),
/// never a panic.
pub fn parse_channel_page(html: &str, channel: &str) -> Vec<TelegramMessage> {
    let doc = Html::parse_document(html);
    // Static selectors: a parse failure here is a programming error.
    let msg_sel = Selector::parse("div.tgme_widget_message").expect("static selector");
    let text_sel = Selector::parse(".tgme_widget_message_text").expect("static selector");
    let date_sel = Selector::parse("a.tgme_widget_message_date time").expect("static selector");
    let views_sel = Selector::parse("span.tgme_widget_message_views").expect("static selector");
    let fwds_sel = Selector::parse("span.tgme_widget_message_forwards").expect("static selector");

    let mut messages = Vec::new();
    for msg in doc.select(&msg_sel) {
        // data-post="channel/441" — the canonical message id.
        let (post_channel, id) = match msg
            .attr("data-post")
            .and_then(|p| p.rsplit_once('/'))
            .and_then(|(c, id)| id.parse::<i64>().ok().map(|id| (c, id)))
        {
            Some(v) => v,
            None => continue,
        };
        // The page is single-channel; use the id, keep our channel label.
        let _ = post_channel;
        let text = first(msg, &text_sel).map(message_text).unwrap_or_default();
        let date = first(msg, &date_sel)
            .and_then(|t| t.attr("datetime"))
            .map(normalize_datetime)
            .unwrap_or_default();
        let views = first(msg, &views_sel)
            .map(|v| v.text().collect::<String>())
            .and_then(|s| parse_views(&s));
        let forwards = first(msg, &fwds_sel)
            .map(|v| v.text().collect::<String>())
            .and_then(|s| parse_views(&s));
        messages.push(TelegramMessage {
            channel: channel.to_string(),
            message_id: id,
            date,
            text,
            views,
            forwards,
            url: format!("https://t.me/{channel}/{id}"),
        });
    }
    messages
}

async fn fetch_page(client: &reqwest::Client, channel: &str) -> Result<String, FeedError> {
    let url = format!("https://t.me/s/{channel}");
    let resp = client
        .get(&url)
        .header(header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| FeedError::new(FEED, format!("request failed: {e}")))?;
    let status = resp.status();
    if status == StatusCode::NOT_FOUND {
        return Err(FeedError::new(
            FEED,
            format!("channel @{channel} not found or not public"),
        ));
    }
    if !status.is_success() {
        return Err(FeedError::new(FEED, format!("upstream status {status}")));
    }
    resp.text()
        .await
        .map_err(|e| FeedError::new(FEED, format!("read body failed: {e}")))
}

/// Fetch + parse one channel's preview page. Single upstream request;
/// callers are expected to cache per the TTL.
pub async fn fetch(client: &reqwest::Client, channel: &str) -> Result<TelegramResponse, FeedError> {
    let html = fetch_page(client, channel).await?;
    let messages = parse_channel_page(&html, channel);
    if messages.is_empty() {
        tracing::warn!(
            channel,
            "t.me/s preview parsed to zero messages — markup may have changed, or the channel is empty/private"
        );
    }
    Ok(TelegramResponse {
        count: messages.len(),
        generated_at: now_iso8601(),
        channel: channel.to_string(),
        messages,
    })
}

/// Keyless liveness probe: fetch the preview of the first allowlisted
/// channel (or Telegram's own announcement channel) and count messages.
pub async fn probe_detail(client: &reqwest::Client, config: &Config) -> Result<String, FeedError> {
    let channel = config
        .telegram_channels
        .first()
        .cloned()
        .unwrap_or_else(|| "telegram".to_string());
    if !valid_channel_name(&channel) {
        return Err(FeedError::new(FEED, "probe channel name invalid"));
    }
    let r = fetch(client, &channel).await?;
    Ok(format!("{} messages (@{})", r.count, channel))
}

fn json_bytes(body: Vec<u8>, hit: bool) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                HeaderName::from_static("x-cache"),
                if hit { "HIT" } else { "MISS" },
            ),
        ],
        body,
    )
        .into_response()
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
) -> Result<Response, ApiError> {
    // `?channel=all` (or empty) aggregates the effective channel set —
    // the no-input "scan it for me" mode.
    let want_all = q
        .channel
        .as_deref()
        .map(|c| c.trim().to_lowercase())
        .map(|c| c.is_empty() || c == "all")
        .unwrap_or(true);
    if want_all {
        return handler_all(state).await;
    }
    let channel = resolve_channel(q.channel.as_deref(), &state.config)?;
    // Same get→fetch→put TTL semantics as routes::cached_bytes, hand-rolled
    // so an upstream failure surfaces as 503 (honest "Telegram unreachable")
    // rather than the 502 that FeedError→ApiError conversion would give.
    let key = format!("telegram:{channel}");
    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }
    let client = state.client.clone();
    let resp = fetch(&client, &channel)
        .await
        .map_err(|e| ApiError::service_unavailable("telegram upstream failed", e.detail))?;
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), TTL);
    Ok(json_bytes(body, false))
}

/// Aggregated multi-channel response for the no-input scan mode.
#[derive(Debug, Serialize)]
struct TelegramAllResponse {
    channels: Vec<String>,
    count: usize,
    generated_at: String,
    messages: Vec<TelegramMessage>,
    failures: Vec<String>,
}

async fn handler_all(state: AppState) -> Result<Response, ApiError> {
    use futures_util::future::join_all;
    let channels = effective_channels(&state.config);
    let key = "telegram:all".to_string();
    if let Some(body) = state.cache.get(&key) {
        return Ok(json_bytes(body, true));
    }
    let futs = channels.iter().map(|ch| {
        let client = state.client.clone();
        let ch2 = ch.clone();
        let st = state.clone();
        async move {
            let ckey = format!("telegram:{ch2}");
            if let Some(body) = st.cache.get(&ckey) {
                return (
                    ch2,
                    serde_json::from_slice::<TelegramResponse>(&body)
                        .map(|r| r.messages)
                        .unwrap_or_default(),
                    None::<String>,
                );
            }
            match fetch(&client, &ch2).await {
                Ok(r) => {
                    let body = serde_json::to_vec(&r).unwrap_or_default();
                    st.cache.put(ckey, body, TTL);
                    (ch2, r.messages, None)
                }
                Err(e) => (ch2, Vec::new(), Some(e.detail)),
            }
        }
    });
    let results = join_all(futs).await;
    let mut messages = Vec::new();
    let mut failures = Vec::new();
    for (ch, msgs, err) in results {
        if let Some(e) = err {
            failures.push(format!("@{ch}: {e}"));
        }
        messages.extend(msgs);
    }
    // Newest first for the scan view.
    messages.sort_by(|a, b| b.date.cmp(&a.date));
    let resp = TelegramAllResponse {
        channels,
        count: messages.len(),
        generated_at: now_iso8601(),
        messages,
        failures,
    };
    let body = serde_json::to_vec(&resp)
        .map_err(|e| ApiError::bad_gateway("serialize failed", e.to_string()))?;
    state.cache.put(key, body.clone(), TTL);
    Ok(json_bytes(body, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../benches/fixtures/telegram_channel.html");

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
    fn missing_channel_falls_back_to_curated_defaults() {
        let config = Config::test_default();
        // No TELEGRAM_CHANNELS set: the curated defaults supply the channel.
        let ch = resolve_channel(None, &config).unwrap();
        assert!(!ch.is_empty());
        assert!(effective_channels(&config).contains(&ch));
    }

    #[test]
    fn effective_channels_uses_curated_defaults_when_env_empty() {
        let config = Config::test_default();
        let channels = effective_channels(&config);
        // 9 verified channels in data/telegram_channels.json.
        assert_eq!(channels.len(), 9);
        assert!(channels.contains(&"Reuters".to_string()));
    }

    #[test]
    fn non_allowlisted_channel_rejected() {
        let mut config = Config::test_default();
        // "beta1" is a *valid* username shape, so rejection is purely the allowlist.
        config.telegram_channels = vec!["alpha".to_string()];
        assert!(resolve_channel(Some("beta1"), &config).is_err());
        assert_eq!(resolve_channel(Some("alpha"), &config).unwrap(), "alpha");
    }

    #[test]
    fn invalid_channel_name_rejected() {
        let config = Config::test_default();
        assert!(resolve_channel(Some("ab"), &config).is_err()); // too short
        assert!(resolve_channel(Some("no spaces"), &config).is_err());
        assert!(resolve_channel(Some("a-b"), &config).is_err()); // dash illegal
        assert!(resolve_channel(Some("../../etc"), &config).is_err()); // path traversal
    }

    #[test]
    fn allowlist_default_used_when_no_param() {
        let mut config = Config::test_default();
        config.telegram_channels = vec!["alpha".to_string(), "beta1".to_string()];
        assert_eq!(resolve_channel(None, &config).unwrap(), "alpha");
    }

    #[test]
    fn parse_views_cases() {
        assert_eq!(parse_views("1.64M"), Some(1_640_000));
        assert_eq!(parse_views("12.3K"), Some(12_300));
        assert_eq!(parse_views("512"), Some(512));
        assert_eq!(parse_views("2K"), Some(2_000));
        assert_eq!(parse_views(""), None);
        assert_eq!(parse_views("abc"), None);
        assert_eq!(parse_views("-5"), None);
    }

    #[test]
    fn parses_fixture() {
        let msgs = parse_channel_page(FIXTURE, "telegram");
        assert_eq!(msgs.len(), 20);
        let m = &msgs[0];
        assert_eq!(m.channel, "telegram");
        assert_eq!(m.message_id, 441);
        assert_eq!(m.url, "https://t.me/telegram/441");
        assert_eq!(m.date, "2026-05-14T16:08:31Z");
        assert!(
            m.text.contains("Bot-to-Bot"),
            "text was: {}",
            &m.text[..80.min(m.text.len())]
        );
        assert_eq!(m.views, Some(1_640_000));
        // Page order is chronological (oldest first); ids ascend.
        assert!(msgs.windows(2).all(|w| w[0].message_id < w[1].message_id));
    }

    #[test]
    fn empty_page_yields_no_messages() {
        let html = "<html><head></head><body><p>nothing here</p></body></html>";
        assert!(parse_channel_page(html, "telegram").is_empty());
    }

    #[test]
    fn malformed_html_yields_no_messages() {
        assert!(parse_channel_page("<<< not html >>>", "telegram").is_empty());
        assert!(parse_channel_page("", "telegram").is_empty());
    }

    #[test]
    fn message_text_collapses_breaks() {
        let html = r#"<div class="tgme_widget_message" data-post="c/1"><div class="tgme_widget_message_text">one<br/><br/>two <b>bold</b><br/>three</div></div>"#;
        let msgs = parse_channel_page(html, "c");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "one\n\ntwo bold\nthree");
    }
}
