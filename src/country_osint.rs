//! Country OSINT directory: curated per-country OSINT resource packs.
//!
//! `GET /api/country-osint?country=<name|iso2|iso3>` returns one country's
//! resource pack — links grouped by category (search, people, business,
//! government, news, social, finance, collections) plus a coverage badge.
//! `GET /api/country-osint/countries` lists all covered countries with
//! their coverage level; `GET /api/country-osint/global` returns the
//! country-agnostic resources (sanctions, corporate, leaks).
//!
//! The data is static and curated (see `data/country_osint.json` and its
//! provenance notes): resource *names and URLs* were extracted from
//! community collections (Poirot-Osint regional pages,
//! paulpogoda/OSINT-for-countries, jivoi/awesome-osint's national search
//! engines); no prose was copied and nothing here is a live lookup —
//! every entry is a link the analyst opens themselves. Coverage labels are
//! honest: `full` = 10+ resources across 4+ categories, `partial` = 4+,
//! `links` = pointer collections only.

use std::sync::OnceLock;

use axum::{Json, Router, extract::Query, routing::get};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// Curated directory, embedded at compile time like the camera registry.
static COUNTRY_OSINT_JSON: &str = include_str!("../data/country_osint.json");

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Resource {
    name: String,
    url: String,
    note: String,
    access: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Country {
    name: String,
    iso2: String,
    iso3: String,
    coverage: String,
    resource_count: u32,
    categories: std::collections::HashMap<String, Vec<Resource>>,
}

#[derive(Debug, Deserialize)]
struct Dataset {
    #[allow(dead_code)]
    meta: serde_json::Value,
    global: Vec<Resource>,
    countries: Vec<Country>,
}

fn dataset() -> &'static Dataset {
    static DATA: OnceLock<Dataset> = OnceLock::new();
    DATA.get_or_init(|| {
        serde_json::from_str(COUNTRY_OSINT_JSON).expect("data/country_osint.json parses")
    })
}

/// `?country=` query.
#[derive(Debug, Deserialize)]
pub struct CountryQuery {
    pub country: Option<String>,
}

pub(crate) fn find_country(q: &str) -> Option<&'static Country> {
    let q = q.trim().to_lowercase();
    dataset().countries.iter().find(|c| {
        c.name.to_lowercase() == q || c.iso2.to_lowercase() == q || c.iso3.to_lowercase() == q
    })
}

/// `GET /api/country-osint?country=<name|iso2|iso3>`.
///
/// 400 when `country` is missing; 404 when it matches nothing.
pub async fn country_osint(Query(q): Query<CountryQuery>) -> Result<Json<Country>, ApiError> {
    let needle = q.country.ok_or_else(|| {
        ApiError::bad_request(
            "missing country",
            "query param country is required, e.g. ?country=Japan or ?country=JP or ?country=JPN",
        )
    })?;
    find_country(&needle).cloned().map(Json).ok_or_else(|| {
        ApiError::not_found(
            "unknown country",
            format!(
                "no OSINT pack for {:?}; see /api/country-osint/countries for the covered list",
                needle
            ),
        )
    })
}

#[derive(Debug, Serialize)]
pub struct CountrySummary {
    name: String,
    iso2: String,
    iso3: String,
    coverage: String,
    resource_count: u32,
}

/// `GET /api/country-osint/countries` — the covered list with coverage badges.
pub async fn country_list() -> Json<Vec<CountrySummary>> {
    Json(
        dataset()
            .countries
            .iter()
            .map(|c| CountrySummary {
                name: c.name.clone(),
                iso2: c.iso2.clone(),
                iso3: c.iso3.clone(),
                coverage: c.coverage.clone(),
                resource_count: c.resource_count,
            })
            .collect(),
    )
}

#[derive(Debug, Serialize)]
pub struct GlobalResponse {
    count: usize,
    resources: Vec<Resource>,
}

/// `GET /api/country-osint/global` — country-agnostic resources.
pub async fn country_global() -> Json<GlobalResponse> {
    let g = &dataset().global;
    Json(GlobalResponse {
        count: g.len(),
        resources: g.clone(),
    })
}

pub fn routes() -> Router<crate::routes::AppState> {
    Router::new()
        .route("/api/country-osint", get(country_osint))
        .route("/api/country-osint/countries", get(country_list))
        .route("/api/country-osint/global", get(country_global))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_parses_and_has_countries() {
        let d = dataset();
        assert!(d.countries.len() >= 20, "expected 20+ countries");
        assert!(!d.global.is_empty());
    }

    #[test]
    fn lookup_by_name_iso2_iso3() {
        assert!(find_country("Japan").is_some());
        assert!(find_country("jp").is_some());
        assert!(find_country("JPN").is_some());
        assert!(find_country("  united kingdom ").is_some());
        assert!(find_country("Narnia").is_none());
    }

    #[test]
    fn coverage_labels_are_valid() {
        for c in &dataset().countries {
            assert!(
                ["full", "partial", "links"].contains(&c.coverage.as_str()),
                "bad coverage on {}",
                c.name
            );
            assert!(!c.iso2.is_empty() && !c.iso3.is_empty());
        }
    }
}
