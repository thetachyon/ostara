//! Per-post risk scoring: a 0-100 heuristic for a single social-media
//! post or message, built from the same keyword families as the
//! safety-v2 place scorer (`src/risk.rs`) plus direct threat language.
//!
//! This is a TEXT heuristic, not a prediction. A high score means the
//! post's language matches violent-crime / terror / unrest / threat
//! keywords — it does not mean the post describes a real event, and it
//! does not identify who is dangerous. News reporting, condemnation,
//! and figurative language all score. Every consumer must label this
//! as a keyword heuristic.
//!
//! ## Scoring
//!
//! - Each firing family contributes `weight * 100`: violent_crime
//!   0.40, terror_conflict 0.35, violent_unrest 0.25.
//! - +5 per additional distinct keyword within a firing family (cap +15).
//! - Direct threat language (assassination, death threat, kill list,
//!   behead, …) adds a flat +20.
//! - Clamped to 0-100. Levels mirror risk.rs: <25 low, <50 moderate,
//!   <75 high, >=75 severe.
//!
//! Matching is case-insensitive with word boundaries; the same
//! exclusions as safety-v2 apply (no bare "assault", no bare
//! "explosion", no generic "crime").

use serde::{Deserialize, Serialize};

/// One risk family: id, weight, and the keywords that fire it.
struct Family {
    id: &'static str,
    weight: f64,
    keywords: &'static [&'static str],
}

const FAMILIES: [Family; 3] = [
    Family {
        id: "violent_crime",
        weight: 0.40,
        keywords: &[
            "murder",
            "homicide",
            "stabbing",
            "stabbed",
            "kidnapping",
            "kidnapped",
            "armed robbery",
            "sexual assault",
        ],
    },
    Family {
        id: "terror_conflict",
        weight: 0.35,
        keywords: &[
            "bombing",
            "shooting",
            "mass shooting",
            "terrorist attack",
            "airstrike",
        ],
    },
    Family {
        id: "violent_unrest",
        weight: 0.25,
        keywords: &["riot", "violent protest", "clashes", "tear gas", "molotov"],
    },
];

/// Direct threat language — person-directed threat phrasing that is not
/// in the safety families.
const THREAT_KEYWORDS: [&str; 8] = [
    "assassination",
    "assassinate",
    "death threat",
    "kill list",
    "behead",
    "lynch",
    "i will kill",
    "going to kill",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostRisk {
    pub score: f64,
    pub level: String,
    pub families: Vec<String>,
    pub matched_keywords: Vec<String>,
}

/// Case-insensitive word-boundary match for a single keyword (which may
/// itself be a multi-word phrase).
fn contains_word(haystack: &str, needle: &str) -> bool {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    // Phrase: simple substring on the lowered text.
    if n.contains(' ') {
        return h.contains(&n);
    }
    // Single word: boundary check.
    let hb: Vec<char> = h.chars().collect();
    let nb: Vec<char> = n.chars().collect();
    if nb.is_empty() || hb.len() < nb.len() {
        return false;
    }
    for i in 0..=(hb.len() - nb.len()) {
        if hb[i..i + nb.len()] == nb[..] {
            let left_ok = i == 0 || !hb[i - 1].is_alphanumeric();
            let right_ok = i + nb.len() == hb.len() || !hb[i + nb.len()].is_alphanumeric();
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

fn level_for(score: f64) -> &'static str {
    if score >= 75.0 {
        "severe"
    } else if score >= 50.0 {
        "high"
    } else if score >= 25.0 {
        "moderate"
    } else {
        "low"
    }
}

/// Score a post's text 0-100. Pure function, unit-tested.
pub fn score_post(text: &str) -> PostRisk {
    let mut families = Vec::new();
    let mut matched: Vec<String> = Vec::new();
    let mut score = 0.0;

    for fam in &FAMILIES {
        let hits: Vec<&str> = fam
            .keywords
            .iter()
            .filter(|k| contains_word(text, k))
            .copied()
            .collect();
        if !hits.is_empty() {
            families.push(fam.id.to_string());
            score += fam.weight * 100.0;
            // +5 per additional distinct keyword, cap +15.
            score += ((hits.len() - 1).min(3) as f64) * 5.0;
            matched.extend(hits.iter().map(|k| k.to_string()));
        }
    }

    let threat_hits: Vec<&str> = THREAT_KEYWORDS
        .iter()
        .filter(|k| contains_word(text, k))
        .copied()
        .collect();
    if !threat_hits.is_empty() {
        score += 20.0;
        matched.extend(threat_hits.iter().map(|k| k.to_string()));
        if !families.contains(&"threat_language".to_string()) {
            families.push("threat_language".to_string());
        }
    }

    score = score.clamp(0.0, 100.0);
    // Round to one decimal for stable output.
    score = (score * 10.0).round() / 10.0;

    PostRisk {
        score,
        level: level_for(score).to_string(),
        families,
        matched_keywords: matched,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_post_scores_zero() {
        let r = score_post("Markets rallied today on strong earnings.");
        assert_eq!(r.score, 0.0);
        assert_eq!(r.level, "low");
        assert!(r.families.is_empty());
    }

    #[test]
    fn single_family_scores_its_weight() {
        let r = score_post("Police report a riot downtown with tear gas deployed.");
        // violent_unrest 25 + 5 (two keywords) = 30
        assert_eq!(r.score, 30.0);
        assert_eq!(r.level, "moderate");
        assert_eq!(r.families, vec!["violent_unrest"]);
    }

    #[test]
    fn multi_family_sums() {
        let r = score_post("Shooting at the mall; murder suspect in custody.");
        // terror 35 + violent_crime 40 = 75 -> severe
        assert_eq!(r.score, 75.0);
        assert_eq!(r.level, "severe");
    }

    #[test]
    fn threat_language_adds_bonus() {
        let r = score_post("Death threat received by the mayor's office.");
        // threat bonus 20 only
        assert_eq!(r.score, 20.0);
        assert!(r.families.contains(&"threat_language".to_string()));
    }

    #[test]
    fn word_boundaries_avoid_false_positives() {
        // "assault" alone must not fire (naval/figurative); "bombing" inside
        // another word must not fire.
        let r = score_post("The assault ship arrived; a plumbing bombingschedule glitch.");
        assert_eq!(r.score, 0.0);
    }

    #[test]
    fn score_clamps_at_100() {
        let r = score_post(
            "Murder, homicide, stabbing, kidnapping, bombing, shooting, airstrike, riot, clashes, tear gas, molotov, assassination, behead, lynch.",
        );
        assert!(r.score <= 100.0);
        assert_eq!(r.level, "severe");
    }
}
