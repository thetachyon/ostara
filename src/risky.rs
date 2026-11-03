//! Risk-ranked social scan: `/api/scan/risky`.
//!
//! Fetches recent posts from the effective Telegram channel set and from
//! Bluesky (risk-oriented default query), scores each post with the
//! per-post risk heuristic (`src/post_risk.rs`), and returns:
//!
//! - `posts`: top posts sorted by risk score descending
//! - `posters`: top accounts aggregated by author — post count, risky
//!   post count, max/avg score, top keywords
//!
//! No input required: Telegram scans the effective channel list
//! (TELEGRAM_CHANNELS env, else the curated defaults); Bluesky uses a
//! risk-keyword query. This is the "scan it for me" endpoint —
//! per-channel / per-query browsing stays on the dedicated feed panels.
//!
//! ## Labeling
//!
//! Scores are KEYWORD HEURISTICS, not verified threats. News reporting,
//! condemnation, and figurative language all score. Poster aggregation
//! lists accounts whose posts matched risk keywords — it is not a list
//! of threat actors and no account is accused of anything.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use crate::{
    error::ApiError,
    feeds,
    post_risk::{PostRisk, score_post},
    routes::{AppState, cached_bytes},
    time::now_iso8601,
};

pub const DISCLAIMER: &str = "KEYWORD HEURISTIC — NOT VERIFIED THREATS. Risk scores measure whether a post's language matches violent-crime, terror/conflict, unrest, or threat keywords. News reporting about violence, condemnation of threats, historical references, and figurative language all score. Nothing here is a verified threat, and no poster listed is accused of making a threat or being dangerous. Every item requires human review.";

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;
/// Bluesky default: risk-keyword search surfaces scorable posts directly
/// instead of fetching generic news and discarding 95% of it.
const BLUESKY_DEFAULT_QUERY: &str =
    "murder OR shooting OR bombing OR stabbing OR riot OR kidnapping OR airstrike";

#[derive(Debug, Serialize, Clone)]
struct RiskyPost {
    source: String,
    author: String,
    author_url: String,
    text: String,
    post_url: String,
    published: String,
    risk: PostRisk,
}

#[derive(Debug, Serialize)]
struct TopPoster {
    source: String,
    author: String,
    author_url: String,
    post_count: usize,
    risky_post_count: usize,
    max_score: f64,
    avg_score: f64,
    top_keywords: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RiskyScanResponse {
    posts: Vec<RiskyPost>,
    posters: Vec<TopPoster>,
    post_count: usize,
    poster_count: usize,
    sources: Vec<SourceNote>,
    scanned_at: String,
    disclaimer: String,
}

#[derive(Debug, Serialize)]
struct SourceNote {
    source: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Deserialize)]
struct RiskyQuery {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    min_score: Option<f64>,
}

async fn fetch_telegram_posts(state: &AppState) -> (Vec<RiskyPost>, SourceNote) {
    let channels = feeds::telegram::effective_channels(&state.config);
    if channels.is_empty() {
        return (
            Vec::new(),
            SourceNote {
                source: "telegram".into(),
                ok: false,
                detail: "no channels available".into(),
            },
        );
    }
    // Fetch channels in parallel; each has its own 300s cache entry.
    let futs = channels.iter().map(|ch| {
        let client = state.client.clone();
        let ch2 = ch.clone();
        let st = state.clone();
        async move {
            let key = format!("telegram:{ch2}");
            let res = cached_bytes::<feeds::telegram::TelegramResponse, _>(
                &st,
                &key,
                feeds::telegram::TTL,
                || feeds::telegram::fetch(&client, &ch2),
            )
            .await;
            (ch2, res)
        }
    });
    let results = join_all(futs).await;
    let mut posts = Vec::new();
    let mut ok = 0;
    let mut failed = 0;
    for (ch, res) in results {
        match res {
            Ok((body, _)) => {
                ok += 1;
                if let Ok(resp) = serde_json::from_slice::<feeds::telegram::TelegramResponse>(&body)
                {
                    for m in resp.messages {
                        let risk = score_post(&m.text);
                        posts.push(RiskyPost {
                            source: "telegram".into(),
                            author: format!("@{ch}"),
                            author_url: format!("https://t.me/s/{ch}"),
                            text: truncate(&m.text, 400),
                            post_url: m.url.clone(),
                            published: m.date.clone(),
                            risk,
                        });
                    }
                }
            }
            Err(_) => failed += 1,
        }
    }
    let note = SourceNote {
        source: "telegram".into(),
        ok: ok > 0,
        detail: format!("{ok} channels ok, {failed} failed"),
    };
    (posts, note)
}

async fn fetch_bluesky_posts(state: &AppState) -> (Vec<RiskyPost>, SourceNote) {
    let key = format!("bluesky:risky:q={BLUESKY_DEFAULT_QUERY}:limit=50");
    let client = state.client.clone();
    let res = cached_bytes::<feeds::bluesky::BlueskyResponse, _>(
        state,
        &key,
        feeds::bluesky::TTL,
        || feeds::bluesky::fetch(&client, BLUESKY_DEFAULT_QUERY, 50),
    )
    .await;
    match res {
        Ok((body, _)) => {
            let mut posts = Vec::new();
            if let Ok(resp) = serde_json::from_slice::<feeds::bluesky::BlueskyResponse>(&body) {
                for p in resp.posts {
                    let risk = score_post(&p.text);
                    posts.push(RiskyPost {
                        source: "bluesky".into(),
                        author: format!("@{}", p.author_handle),
                        author_url: format!(
                            "https://bsky.app/profile/{}",
                            p.author_handle.trim_start_matches('@')
                        ),
                        text: truncate(&p.text, 400),
                        post_url: p.url.clone(),
                        published: p.created_at.clone(),
                        risk,
                    });
                }
            }
            (
                posts,
                SourceNote {
                    source: "bluesky".into(),
                    ok: true,
                    detail: "ok".into(),
                },
            )
        }
        Err(e) => (
            Vec::new(),
            SourceNote {
                source: "bluesky".into(),
                ok: false,
                detail: e.user_message(),
            },
        ),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Aggregate posts into per-poster summaries, ranked by max score then
/// risky-post count. Only posters with at least one risky post
/// (score >= min_score) appear.
fn top_posters(posts: &[RiskyPost], min_score: f64) -> Vec<TopPoster> {
    #[derive(Default)]
    struct Agg<'a> {
        source: String,
        author_url: String,
        post_count: usize,
        risky: Vec<&'a RiskyPost>,
        kw_counts: HashMap<String, usize>,
    }
    let mut map: HashMap<(String, String), Agg> = HashMap::new();
    for p in posts {
        let e = map
            .entry((p.source.clone(), p.author.clone()))
            .or_insert_with(|| Agg {
                source: p.source.clone(),
                author_url: p.author_url.clone(),
                ..Default::default()
            });
        e.post_count += 1;
        if p.risk.score >= min_score {
            e.risky.push(p);
            for k in &p.risk.matched_keywords {
                *e.kw_counts.entry(k.clone()).or_insert(0) += 1;
            }
        }
    }
    let mut out: Vec<TopPoster> = map
        .into_iter()
        .filter(|(_, a)| !a.risky.is_empty())
        .map(|((_, author), a)| {
            let max_score = a.risky.iter().map(|p| p.risk.score).fold(0.0, f64::max);
            let avg_score =
                a.risky.iter().map(|p| p.risk.score).sum::<f64>() / a.risky.len() as f64;
            let mut kws: Vec<(String, usize)> = a.kw_counts.into_iter().collect();
            kws.sort_by(|x, y| y.1.cmp(&x.1).then(x.0.cmp(&y.0)));
            TopPoster {
                source: a.source,
                author,
                author_url: a.author_url,
                post_count: a.post_count,
                risky_post_count: a.risky.len(),
                max_score: (max_score * 10.0).round() / 10.0,
                avg_score: (avg_score * 10.0).round() / 10.0,
                top_keywords: kws.into_iter().take(5).map(|(k, _)| k).collect(),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.max_score
            .partial_cmp(&a.max_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.risky_post_count.cmp(&a.risky_post_count))
    });
    out
}

async fn scan(
    State(state): State<AppState>,
    Query(q): Query<RiskyQuery>,
) -> Result<Json<RiskyScanResponse>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let min_score = q.min_score.unwrap_or(25.0).clamp(0.0, 100.0);

    let (tg_posts, tg_note) = fetch_telegram_posts(&state).await;
    let (bs_posts, bs_note) = fetch_bluesky_posts(&state).await;

    let mut posts: Vec<RiskyPost> = tg_posts.into_iter().chain(bs_posts).collect();
    // Highest risk first; ties broken by source for stability.
    posts.sort_by(|a, b| {
        b.risk
            .score
            .partial_cmp(&a.risk.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.source.cmp(&b.source))
    });
    let posters = top_posters(&posts, min_score);
    let risky_posts: Vec<RiskyPost> = posts
        .into_iter()
        .filter(|p| p.risk.score >= min_score)
        .take(limit)
        .collect();
    let poster_count = posters.len();
    let top_posters_capped: Vec<TopPoster> = posters.into_iter().take(limit).collect();

    Ok(Json(RiskyScanResponse {
        post_count: risky_posts.len(),
        poster_count,
        posts: risky_posts,
        posters: top_posters_capped,
        sources: vec![tg_note, bs_note],
        scanned_at: now_iso8601(),
        disclaimer: DISCLAIMER.into(),
    }))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/scan/risky", get(scan))
        .route("/api/scan/threat-posters", get(threat_posters))
}

// ---------------------------------------------------------------------------
// Threat posters: accounts posting threat language directed at someone.
// ---------------------------------------------------------------------------

pub const THREAT_POSTER_DISCLAIMER: &str = "KEYWORD HEURISTIC — NOT VERIFIED THREATS. These are accounts whose public posts matched threat language (assassination, death threat, kill, etc.). Targets are extracted heuristically from phrasing like 'threats against X' and may be wrong. News reporting about threats, condemnation, and figurative language all match. Curated news outlets are excluded from this view. No account listed is accused of making a genuine threat or being dangerous. Every item requires human review.";

const THREAT_BLUESKY_QUERY: &str =
    "assassination OR \"death threat\" OR \"kill list\" OR behead OR \"threats against\"";

#[derive(Debug, Serialize)]
struct ThreatPoster {
    source: String,
    author: String,
    author_url: String,
    threat_post_count: usize,
    targets: Vec<String>,
    max_score: f64,
    example_posts: Vec<ThreatExample>,
}

#[derive(Debug, Serialize, Clone)]
struct ThreatExample {
    text: String,
    post_url: String,
    published: String,
    targets: Vec<String>,
    score: f64,
}

#[derive(Debug, Serialize)]
struct ThreatPostersResponse {
    posters: Vec<ThreatPoster>,
    poster_count: usize,
    sources: Vec<SourceNote>,
    scanned_at: String,
    disclaimer: String,
}

/// Heuristic target extraction: who is the threat directed at?
/// Looks for "against X", "to X" / "toward(s) X" after threat language,
/// and "<threat verb> X" with X a capitalized phrase. Returns deduped
/// candidate names; may be empty or wrong — labeled heuristic.
fn extract_targets(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    let is_name_word = |w: &str| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        w.chars().next().is_some_and(|c| c.is_uppercase())
            && w.len() > 1
            && !["I", "The", "A", "An"].contains(&w)
    };
    // Trigger words that precede a target: prepositions and threat verbs.
    let triggers = [
        "against",
        "toward",
        "towards",
        "kill",
        "kills",
        "killed",
        "assassinate",
        "assassinates",
        "assassinated",
        "murder",
        "murders",
        "murdered",
        "behead",
        "beheads",
        "beheaded",
        "lynch",
    ];
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    for (i, w) in lower.iter().enumerate() {
        let w_clean = w.trim_matches(|c: char| !c.is_alphanumeric());
        if !triggers.contains(&w_clean) {
            continue;
        }
        // Capture up to 3 following capitalized words as the target name.
        // Skip a leading "the".
        let mut j = i + 1;
        if j < words.len() && words[j].to_lowercase() == "the" {
            j += 1;
        }
        let mut name_parts: Vec<String> = Vec::new();
        while j < words.len() && name_parts.len() < 3 && is_name_word(words[j]) {
            name_parts.push(
                words[j]
                    .trim_matches(|c: char| !c.is_alphanumeric())
                    .to_string(),
            );
            j += 1;
        }
        if !name_parts.is_empty() {
            let name = name_parts.join(" ");
            if !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct ThreatPostersQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// Author handles of the curated news channels (`@{name}`), excluded from
/// threat-poster results — news outlets reporting on violence are not
/// threat actors.
fn news_channel_authors() -> Vec<String> {
    crate::feeds::telegram::curated_channel_names()
        .into_iter()
        .map(|c| format!("@{c}"))
        .collect()
}

async fn threat_posters(
    State(state): State<AppState>,
    Query(q): Query<ThreatPostersQuery>,
) -> Result<Json<ThreatPostersResponse>, ApiError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 50);

    // Threat-keyword posts from both sources.
    let (tg_posts, tg_note) = {
        let (posts, note) = fetch_telegram_posts(&state).await;
        let filtered: Vec<RiskyPost> = posts
            .into_iter()
            .filter(|p| p.risk.families.contains(&"threat_language".to_string()))
            .collect();
        (filtered, note)
    };
    let (bs_posts, bs_note) = {
        let key = format!("bluesky:threats:q={THREAT_BLUESKY_QUERY}:limit=50");
        let client = state.client.clone();
        let res = cached_bytes::<feeds::bluesky::BlueskyResponse, _>(
            &state,
            &key,
            feeds::bluesky::TTL,
            || feeds::bluesky::fetch(&client, THREAT_BLUESKY_QUERY, 50),
        )
        .await;
        let mut posts = Vec::new();
        let note = match res {
            Ok((body, _)) => {
                if let Ok(resp) = serde_json::from_slice::<feeds::bluesky::BlueskyResponse>(&body) {
                    for p in resp.posts {
                        let risk = score_post(&p.text);
                        if risk.families.contains(&"threat_language".to_string()) {
                            posts.push(RiskyPost {
                                source: "bluesky".into(),
                                author: format!("@{}", p.author_handle),
                                author_url: format!(
                                    "https://bsky.app/profile/{}",
                                    p.author_handle.trim_start_matches('@')
                                ),
                                text: truncate(&p.text, 400),
                                post_url: p.url.clone(),
                                published: p.created_at.clone(),
                                risk,
                            });
                        }
                    }
                }
                SourceNote {
                    source: "bluesky".into(),
                    ok: true,
                    detail: "ok".into(),
                }
            }
            Err(e) => SourceNote {
                source: "bluesky".into(),
                ok: false,
                detail: e.user_message(),
            },
        };
        (posts, note)
    };

    // Aggregate by author, excluding curated news outlets (they report on
    // violence; they are not threat actors).
    let news_authors = news_channel_authors();
    let mut map: HashMap<(String, String), Vec<RiskyPost>> = HashMap::new();
    for p in tg_posts.into_iter().chain(bs_posts) {
        if p.source == "telegram" && news_authors.contains(&p.author) {
            continue;
        }
        map.entry((p.source.clone(), p.author.clone()))
            .or_default()
            .push(p);
    }
    let mut posters: Vec<ThreatPoster> = map
        .into_iter()
        .map(|((source, author), mut posts)| {
            posts.sort_by(|a, b| {
                b.risk
                    .score
                    .partial_cmp(&a.risk.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut targets: Vec<String> = Vec::new();
            let examples: Vec<ThreatExample> = posts
                .iter()
                .take(3)
                .map(|p| {
                    let t = extract_targets(&p.text);
                    for name in &t {
                        if !targets.contains(name) {
                            targets.push(name.clone());
                        }
                    }
                    ThreatExample {
                        text: p.text.clone(),
                        post_url: p.post_url.clone(),
                        published: p.published.clone(),
                        targets: t,
                        score: p.risk.score,
                    }
                })
                .collect();
            let max_score = posts.iter().map(|p| p.risk.score).fold(0.0, f64::max);
            ThreatPoster {
                author_url: posts[0].author_url.clone(),
                source,
                author,
                threat_post_count: posts.len(),
                targets,
                max_score: (max_score * 10.0).round() / 10.0,
                example_posts: examples,
            }
        })
        .collect();
    posters.sort_by(|a, b| {
        b.threat_post_count.cmp(&a.threat_post_count).then(
            b.max_score
                .partial_cmp(&a.max_score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    let poster_count = posters.len();
    posters.truncate(limit);

    Ok(Json(ThreatPostersResponse {
        posters,
        poster_count,
        sources: vec![tg_note, bs_note],
        scanned_at: now_iso8601(),
        disclaimer: THREAT_POSTER_DISCLAIMER.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post(source: &str, author: &str, text: &str) -> RiskyPost {
        RiskyPost {
            source: source.into(),
            author: author.into(),
            author_url: String::new(),
            text: text.into(),
            post_url: String::new(),
            published: String::new(),
            risk: score_post(text),
        }
    }

    #[test]
    fn posters_rank_by_max_score_then_count() {
        let posts = vec![
            post("telegram", "@a", "Riot with tear gas downtown."),
            post("telegram", "@a", "Markets up today."),
            post("bluesky", "@b", "Shooting reported near the mall."),
            post(
                "bluesky",
                "@b",
                "Bombing and shooting, murder suspect held.",
            ),
        ];
        let top = top_posters(&posts, 25.0);
        assert_eq!(top.len(), 2);
        // @b has the 75-pointer; @a tops at 30.
        assert_eq!(top[0].author, "@b");
        assert_eq!(top[0].risky_post_count, 2);
        assert_eq!(top[1].author, "@a");
        assert_eq!(top[1].post_count, 2);
        assert_eq!(top[1].risky_post_count, 1);
    }

    #[test]
    fn posters_exclude_clean_accounts() {
        let posts = vec![post("telegram", "@clean", "Sunny weather today.")];
        assert!(top_posters(&posts, 25.0).is_empty());
    }

    #[test]
    fn disclaimer_labels_heuristic() {
        assert!(DISCLAIMER.contains("KEYWORD HEURISTIC"));
        assert!(DISCLAIMER.contains("no poster listed is accused"));
    }

    #[test]
    fn extract_targets_finds_against_phrase() {
        let t = extract_targets("Police investigate death threats against Emmanuel Macron.");
        assert!(t.contains(&"Emmanuel Macron".to_string()), "{t:?}");
    }

    #[test]
    fn extract_targets_finds_verb_target() {
        let t = extract_targets("Online posts call to assassinate the President.");
        assert!(t.contains(&"President".to_string()), "{t:?}");
    }

    #[test]
    fn extract_targets_empty_without_trigger() {
        let t = extract_targets("Markets rallied on strong earnings today.");
        assert!(t.is_empty());
    }

    #[test]
    fn extract_targets_dedupes() {
        let t = extract_targets("Threats against Macron and threats against Macron again.");
        assert_eq!(t.iter().filter(|x| *x == "Macron").count(), 1);
    }

    #[test]
    fn news_channel_authors_lists_curated_outlets() {
        let authors = news_channel_authors();
        assert_eq!(authors.len(), 9);
        assert!(authors.contains(&"@Reuters".to_string()));
        assert!(authors.contains(&"@bbcnews".to_string()));
    }
}
