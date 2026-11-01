//! Entity graph: basic link analysis over entities surfaced by the feeds
//! (Phase 4 — BASIC parity with OSIRIS's link view).
//!
//! The graph is rebuilt per request from fresh feed snapshots; there is no
//! persistence (documented v1 limitation). Edges are co-occurrence *within a
//! single feed item* — an aircraft's hex, callsign, and registration from
//! the same adsb.lol record, a NORAD id and the satellite name on the same
//! TLE line, a facility and its country from the same registry row, an IOC
//! and its threat type from the same threat-intel record. Nothing fancier:
//! cross-feed entity resolution, temporal edges, and weighted fusion are the
//! future differentiator and stay intentionally out of scope here.
//!
//! Fetch policy: modest limits (flights 250), one `tokio::join!` fan-out,
//! and a failing feed contributes no nodes but never fails the graph build.

use std::collections::{HashMap, HashSet, VecDeque};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{error::ApiError, feeds, routes::AppState, time::now_iso8601};

// ---------------------------------------------------------------------------
// Entity model
// ---------------------------------------------------------------------------

/// Entity kinds. The seven briefed kinds (`aircraft`, `callsign`,
/// `norad_id`, `facility`, `domain`, `ip`, `cve`) cover the directly
/// queryable identifiers; the graph also needs nodes for satellite names,
/// nuclear countries, and threat families, which get the three auxiliary
/// kinds below. Every kind is serialized as a plain string so callers can
/// match on it without an enum contract.
pub const KIND_AIRCRAFT: &str = "aircraft";
pub const KIND_CALLSIGN: &str = "callsign";
pub const KIND_NORAD_ID: &str = "norad_id";
pub const KIND_FACILITY: &str = "facility";
pub const KIND_DOMAIN: &str = "domain";
pub const KIND_IP: &str = "ip";
pub const KIND_CVE: &str = "cve";
/// Satellite names (e.g. "ISS (ZARYA)") — auxiliary kind, see module docs.
pub const KIND_NAME: &str = "name";
/// Nuclear countries (e.g. "France") — auxiliary kind, see module docs.
pub const KIND_COUNTRY: &str = "country";
/// Cyber threat families (e.g. "botnet_cc") — auxiliary kind, see module docs.
pub const KIND_THREAT_TYPE: &str = "threat_type";

// Relation labels. `add_link(a, b, rel, feed)` records "b is <rel> of a";
// [`inverse_relation`] flips the label when listing neighbors from b's side.
const REL_CALLSIGN_OF: &str = "callsign-of";
const REL_HEX_OF: &str = "hex-of";
const REL_REGISTRATION_OF: &str = "registration-of";
const REL_NAME_OF: &str = "name-of";
const REL_NORAD_ID_OF: &str = "norad-id-of";
const REL_LOCATED_IN: &str = "located-in";
const REL_HOSTS: &str = "hosts";
const REL_THREAT_TYPE_OF: &str = "threat-type-of";
const REL_INDICATOR_OF: &str = "indicator-of";

fn inverse_relation(rel: &str) -> &str {
    match rel {
        REL_CALLSIGN_OF => REL_HEX_OF,
        REL_REGISTRATION_OF => REL_HEX_OF,
        REL_HEX_OF => REL_CALLSIGN_OF,
        REL_NAME_OF => REL_NORAD_ID_OF,
        REL_NORAD_ID_OF => REL_NAME_OF,
        REL_LOCATED_IN => REL_HOSTS,
        REL_HOSTS => REL_LOCATED_IN,
        REL_THREAT_TYPE_OF => REL_INDICATOR_OF,
        REL_INDICATOR_OF => REL_THREAT_TYPE_OF,
        _ => "co-occurs",
    }
}

/// One graph node: a typed identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Entity {
    pub kind: String,
    pub value: String,
}

impl Entity {
    pub fn new(kind: &str, value: &str) -> Self {
        Self {
            kind: kind.to_string(),
            value: value.to_string(),
        }
    }

    /// Node identity key: kind + case-folded value, so `ISS (ZARYA)` and
    /// `iss (zarya)` resolve to the same node.
    fn key(&self) -> (String, String) {
        (self.kind.clone(), self.value.to_lowercase())
    }
}

/// One undirected edge, created from a single feed item.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    pub from: Entity,
    pub to: Entity,
    /// "to is <relation> of from" (e.g. `callsign-of`).
    pub relation: String,
    /// Feed the co-occurrence came from (`feeds::*::FEED` or a literal).
    pub source_feed: String,
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

/// In-memory co-occurrence graph. Small by construction (hundreds of nodes),
/// so adjacency is computed by scanning edges — no index to maintain.
#[derive(Debug, Default)]
pub struct Graph {
    nodes: HashMap<(String, String), Entity>,
    edges: Vec<Edge>,
    /// Dedupe key for undirected (a, b, relation) triples.
    seen: HashSet<(String, String, String, String, String)>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a node (idempotent); returns its identity key.
    pub fn add_entity(&mut self, kind: &str, value: &str) -> Option<(String, String)> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let e = Entity::new(kind, value);
        let key = e.key();
        self.nodes.entry(key.clone()).or_insert(e);
        Some(key)
    }

    /// Insert both endpoints and one undirected edge between them
    /// (idempotent on the undirected (a, b, relation) triple).
    pub fn add_link(
        &mut self,
        a_kind: &str,
        a_value: &str,
        b_kind: &str,
        b_value: &str,
        relation: &str,
        source_feed: &str,
    ) {
        let (Some(ak), Some(bk)) = (
            self.add_entity(a_kind, a_value),
            self.add_entity(b_kind, b_value),
        ) else {
            return;
        };
        // Canonical key order so (a,b) and (b,a) dedupe to one edge; the
        // stored edge keeps creation order so the relation label stays
        // oriented ("to is <relation> of from").
        let (ck1, ck2) = if ak <= bk { (&ak, &bk) } else { (&bk, &ak) };
        let dedupe = (
            ck1.0.clone(),
            ck1.1.clone(),
            ck2.0.clone(),
            ck2.1.clone(),
            relation.to_string(),
        );
        if !self.seen.insert(dedupe) {
            return;
        }
        // Both keys were just inserted, so these unwraps cannot fail.
        let a = self.nodes.get(&ak).cloned().unwrap();
        let b = self.nodes.get(&bk).cloned().unwrap();
        self.edges.push(Edge {
            from: a,
            to: b,
            relation: relation.to_string(),
            source_feed: source_feed.to_string(),
        });
    }

    /// Resolve a query string to a node, case-insensitively, across every
    /// entity type. The caller learns the kind from the response.
    pub fn lookup(&self, value: &str) -> Option<&Entity> {
        let want = value.trim().to_lowercase();
        if want.is_empty() {
            return None;
        }
        self.nodes.values().find(|e| e.value.to_lowercase() == want)
    }

    /// Direct neighbors of a node, with the relation label oriented so it
    /// reads from the neighbor toward `entity` ("TREK136 is callsign-of
    /// ae1450"). Deterministic order: by (kind, value).
    pub fn neighbors(&self, entity: &Entity) -> Vec<Neighbor> {
        let mut out = Vec::new();
        for e in &self.edges {
            if e.from == *entity {
                out.push(Neighbor {
                    entity: e.to.value.clone(),
                    kind: e.to.kind.clone(),
                    relation: e.relation.clone(),
                    source_feed: e.source_feed.clone(),
                });
            } else if e.to == *entity {
                out.push(Neighbor {
                    entity: e.from.value.clone(),
                    kind: e.from.kind.clone(),
                    relation: inverse_relation(&e.relation).to_string(),
                    source_feed: e.source_feed.clone(),
                });
            }
        }
        out.sort_by(|a, b| (&a.kind, &a.entity).cmp(&(&b.kind, &b.entity)));
        out
    }

    /// BFS expansion from `entity` to `depth` hops (depth ≥ 1). Returns the
    /// visited nodes and the induced subgraph's edges, both sorted for
    /// deterministic output.
    pub fn expand(&self, entity: &Entity, depth: u32) -> Expanded {
        let mut visited: HashSet<(String, String)> = HashSet::new();
        let mut queue: VecDeque<(Entity, u32)> = VecDeque::new();
        visited.insert(entity.key());
        queue.push_back((entity.clone(), 0));
        while let Some((node, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            for e in &self.edges {
                let other = if e.from == node {
                    Some(&e.to)
                } else if e.to == node {
                    Some(&e.from)
                } else {
                    None
                };
                if let Some(other) = other
                    && visited.insert(other.key())
                {
                    queue.push_back((other.clone(), d + 1));
                }
            }
        }
        let mut nodes: Vec<Entity> = visited
            .iter()
            .filter_map(|k| self.nodes.get(k).cloned())
            .collect();
        nodes.sort_by(|a, b| (&a.kind, &a.value).cmp(&(&b.kind, &b.value)));
        let mut edges: Vec<Edge> = self
            .edges
            .iter()
            .filter(|e| visited.contains(&e.from.key()) && visited.contains(&e.to.key()))
            .cloned()
            .collect();
        edges.sort_by(|a, b| {
            (
                a.from.kind.clone(),
                a.from.value.clone(),
                a.to.kind.clone(),
                a.to.value.clone(),
                a.relation.clone(),
            )
                .cmp(&(
                    b.from.kind.clone(),
                    b.from.value.clone(),
                    b.to.kind.clone(),
                    b.to.value.clone(),
                    b.relation.clone(),
                ))
        });
        Expanded { nodes, edges }
    }
}

/// BFS result: visited nodes + edges of the induced subgraph.
#[derive(Debug)]
pub struct Expanded {
    pub nodes: Vec<Entity>,
    pub edges: Vec<Edge>,
}

// ---------------------------------------------------------------------------
// Feed -> graph ingestion (co-occurrence within a single feed item only)
// ---------------------------------------------------------------------------

/// Flights: hex/icao24 ↔ callsign ↔ tail number, one link per pair from the
/// same aircraft record.
pub fn add_flights(graph: &mut Graph, flights: &[feeds::flights::Flight]) {
    for f in flights {
        if let Some(cs) = f.callsign.as_deref() {
            graph.add_link(
                KIND_AIRCRAFT,
                &f.hex,
                KIND_CALLSIGN,
                cs,
                REL_CALLSIGN_OF,
                feeds::flights::FEED,
            );
        }
        if let Some(reg) = f.registration.as_deref() {
            graph.add_link(
                KIND_AIRCRAFT,
                &f.hex,
                KIND_AIRCRAFT,
                reg,
                REL_REGISTRATION_OF,
                feeds::flights::FEED,
            );
        }
    }
}

/// CelesTrak TLE satellites: norad_id ↔ satellite name.
pub fn add_satellites(graph: &mut Graph, sats: &[feeds::satellites::Satellite]) {
    for s in sats {
        if s.norad_id == 0 {
            continue;
        }
        graph.add_link(
            KIND_NORAD_ID,
            &s.norad_id.to_string(),
            KIND_NAME,
            &s.name,
            REL_NAME_OF,
            feeds::satellites::FEED,
        );
    }
}

/// n2yo catalog satellites: same shape as TLE (norad_id ↔ name), attributed
/// to the n2yo source. Dedupe is free — nodes are keyed by (kind, value).
pub fn add_catalog(graph: &mut Graph, sats: &[feeds::n2yo::CatalogSat]) {
    for s in sats {
        if s.norad_id == 0 {
            continue;
        }
        graph.add_link(
            KIND_NORAD_ID,
            &s.norad_id.to_string(),
            KIND_NAME,
            &s.name,
            REL_NAME_OF,
            feeds::n2yo::FEED,
        );
    }
}

/// Nuclear registry: facility name ↔ country.
pub fn add_nuclear(graph: &mut Graph, facilities: &[feeds::nuclear::NuclearFacility]) {
    for f in facilities {
        graph.add_link(
            KIND_FACILITY,
            &f.name,
            KIND_COUNTRY,
            &f.country,
            REL_LOCATED_IN,
            feeds::nuclear::FEED,
        );
    }
}

/// Map a cyber indicator record onto a graph entity kind. URL indicators are
/// reduced to their host (domain or IP); ThreatFox IOCs carry their kind
/// directly. Returns `None` for values with no usable host.
fn indicator_entity(kind_desc: &str, value: &str) -> Option<(String, String)> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if v.to_ascii_uppercase().starts_with("CVE-") {
        return Some((KIND_CVE.to_string(), v.to_string()));
    }
    let kd = kind_desc.to_lowercase();
    if kd == "url" {
        let host = strip_url_host(v)?;
        if is_ipv4(&host) {
            return Some((KIND_IP.to_string(), host));
        }
        return Some((KIND_DOMAIN.to_string(), host));
    }
    if kd.contains("ip") {
        // "ip:port" / "ip" — port stripped, v4 only for simplicity.
        let host = v.split(':').next().unwrap_or(v).trim().to_string();
        return Some((KIND_IP.to_string(), host));
    }
    if kd.contains("domain") || kd.contains("host") {
        return Some((KIND_DOMAIN.to_string(), v.to_string()));
    }
    None
}

/// Extract the host from a URL string without a URL parser: strip the
/// scheme, then cut at the first `/`, `:`, `?`, or `#`.
fn strip_url_host(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = after_scheme
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// Cyber: indicator value ↔ threat type (capped — the feed window can be
/// large; co-occurrence only, no cross-feed resolution).
pub fn add_cyber(graph: &mut Graph, indicators: &[feeds::cyber::Indicator], limit: usize) {
    for ind in indicators.iter().take(limit) {
        let Some((kind, value)) = indicator_entity(&ind.kind, &ind.value) else {
            continue;
        };
        let threat = ind.threat_type.trim();
        if threat.is_empty() {
            continue;
        }
        let threat_kind = if threat.to_ascii_uppercase().starts_with("CVE-") {
            KIND_CVE
        } else {
            KIND_THREAT_TYPE
        };
        graph.add_link(
            &kind,
            &value,
            threat_kind,
            threat,
            REL_THREAT_TYPE_OF,
            feeds::cyber::FEED,
        );
    }
}

// ---------------------------------------------------------------------------
// Live graph build
// ---------------------------------------------------------------------------

const FLIGHTS_LIMIT: usize = 250;
const SAT_TLE_LIMIT: usize = 100;
/// Cyber co-occurrence is bounded before ingest: the threat-intel window is
/// large and IOCs are the noisiest entity type.
const CYBER_GRAPH_CAP: usize = 100;

/// Build the graph from fresh snapshots: flights, TLE, n2yo catalog, the
/// embedded nuclear registry, and cyber indicators — one `tokio::join!`
/// fan-out. A failing feed contributes no nodes but never fails the build;
/// the nuclear registry is static data and is always present.
pub async fn build_live_graph(client: &reqwest::Client, config: &crate::config::Config) -> Graph {
    let (flights, tle, catalog, cyber) = tokio::join!(
        feeds::flights::fetch(client, "mil", FLIGHTS_LIMIT),
        feeds::satellites::fetch(client, SAT_TLE_LIMIT),
        feeds::n2yo::fetch(client, config, 0.0, 0.0),
        // The cyber feed is keyed (abuse.ch Auth-Key); without a key it
        // contributes nothing — its IOCs (incl. ThreatFox's) flow into the
        // graph automatically when the key is set.
        async {
            match &config.threatfox_api_key {
                Some(key) => feeds::cyber::fetch(client, key).await,
                None => Err(crate::error::FeedError::new(
                    feeds::cyber::FEED,
                    "disabled: THREATFOX_API_KEY unset",
                )),
            }
        },
    );
    let mut graph = Graph::new();
    if let Ok(r) = flights {
        add_flights(&mut graph, &r.flights);
    }
    if let Ok(r) = tle {
        add_satellites(&mut graph, &r.satellites);
    }
    if let Ok(r) = catalog {
        add_catalog(&mut graph, &r.satellites);
    }
    if let Ok(registry) = feeds::nuclear::load_registry() {
        add_nuclear(&mut graph, &registry);
    }
    if let Ok(r) = cyber {
        add_cyber(&mut graph, &r.indicators, CYBER_GRAPH_CAP);
    }
    graph
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Neighbor {
    pub entity: String,
    pub kind: String,
    pub relation: String,
    pub source_feed: String,
}

#[derive(Debug, Serialize)]
pub struct GraphDetailResponse {
    pub entity: String,
    pub kind: String,
    pub neighbors: Vec<Neighbor>,
    pub generated_at: String,
}

#[derive(Debug, Serialize)]
pub struct ExpandNode {
    pub entity: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct ExpandEdge {
    pub from: ExpandNode,
    pub to: ExpandNode,
    pub relation: String,
    pub source_feed: String,
}

#[derive(Debug, Serialize)]
pub struct ExpandResponse {
    pub entity: String,
    pub depth: u32,
    pub nodes: Vec<ExpandNode>,
    pub edges: Vec<ExpandEdge>,
    pub generated_at: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct GraphQuery {
    pub entity: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExpandQuery {
    pub entity: Option<String>,
    pub depth: Option<u32>,
}

fn require_entity(q: &Option<String>) -> Result<String, ApiError> {
    match q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => Ok(e.to_string()),
        None => Err(ApiError::bad_request(
            "missing entity",
            "query param entity is required, e.g. ?entity=25544 or ?entity=N123AB",
        )),
    }
}

/// `ApiError` has no 404 constructor (its variants are 400/502/500/503),
/// and mapping "entity not found" to 500 would be dishonest — so the
/// not-found case is a hand-built 404 JSON response instead.
fn not_found(entity: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "entity not found",
            "detail": format!(
                "no graph node for entity {entity:?}; the graph is rebuilt per request from current feed snapshots, so a missing entity may simply be absent from this snapshot"
            ),
        })),
    )
        .into_response()
}

/// GET /api/graph?entity= — node detail + its direct neighbors.
pub async fn detail(
    State(state): State<AppState>,
    Query(q): Query<GraphQuery>,
) -> Result<Response, ApiError> {
    let entity_value = require_entity(&q.entity)?;
    let graph = build_live_graph(&state.client, &state.config).await;
    let Some(node) = graph.lookup(&entity_value) else {
        return Ok(not_found(&entity_value));
    };
    let neighbors = graph.neighbors(node);
    Ok(Json(GraphDetailResponse {
        entity: node.value.clone(),
        kind: node.kind.clone(),
        neighbors,
        generated_at: now_iso8601(),
    })
    .into_response())
}

/// GET /api/graph/expand?entity=&depth= — BFS expansion, depth default 1,
/// clamped to 1..=3 (the graph is snapshot-sized; deeper would fan out).
pub async fn expand(
    State(state): State<AppState>,
    Query(q): Query<ExpandQuery>,
) -> Result<Response, ApiError> {
    let entity_value = require_entity(&q.entity)?;
    let depth = q.depth.unwrap_or(1).clamp(1, 3);
    let graph = build_live_graph(&state.client, &state.config).await;
    let Some(node) = graph.lookup(&entity_value) else {
        return Ok(not_found(&entity_value));
    };
    let expanded = graph.expand(node, depth);
    Ok(Json(ExpandResponse {
        entity: node.value.clone(),
        depth,
        nodes: expanded
            .nodes
            .iter()
            .map(|n| ExpandNode {
                entity: n.value.clone(),
                kind: n.kind.clone(),
            })
            .collect(),
        edges: expanded
            .edges
            .iter()
            .map(|e| ExpandEdge {
                from: ExpandNode {
                    entity: e.from.value.clone(),
                    kind: e.from.kind.clone(),
                },
                to: ExpandNode {
                    entity: e.to.value.clone(),
                    kind: e.to.kind.clone(),
                },
                relation: e.relation.clone(),
                source_feed: e.source_feed.clone(),
            })
            .collect(),
        generated_at: now_iso8601(),
    })
    .into_response())
}

/// Router for the Phase-4 entity graph. State type matches `routes::app`
/// so the parent merges it into the canonical router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/graph", get(detail))
        .route("/api/graph/expand", get(expand))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feeds::{
        cyber::Indicator, flights::Flight, n2yo::CatalogSat, nuclear::NuclearFacility,
        satellites::Satellite,
    };

    fn flight(hex: &str, callsign: Option<&str>, reg: Option<&str>) -> Flight {
        Flight {
            hex: hex.to_string(),
            callsign: callsign.map(str::to_string),
            registration: reg.map(str::to_string),
            aircraft_type: None,
            latitude: None,
            longitude: None,
            altitude_m: None,
            on_ground: false,
            speed_knots: None,
            track_deg: None,
            emergency: None,
            seen_pos_s: None,
        }
    }

    fn sat(norad_id: u32, name: &str) -> Satellite {
        Satellite {
            name: name.to_string(),
            norad_id,
            line1: String::new(),
            line2: String::new(),
        }
    }

    fn facility(name: &str, country: &str) -> NuclearFacility {
        NuclearFacility {
            id: "x".to_string(),
            name: name.to_string(),
            lat: 0.0,
            lon: 0.0,
            country: country.to_string(),
            reactor_type: None,
            status: "operational".to_string(),
            source: "test".to_string(),
        }
    }

    fn indicator(kind: &str, value: &str, threat: &str) -> Indicator {
        Indicator {
            kind: kind.to_string(),
            value: value.to_string(),
            threat_type: threat.to_string(),
            first_seen: String::new(),
        }
    }

    fn fixture_graph() -> Graph {
        let mut g = Graph::new();
        add_flights(
            &mut g,
            &[
                flight("ae1450", Some("TREK136"), Some("N123AB")),
                flight("abc123", Some("TREK137"), None),
            ],
        );
        add_satellites(&mut g, &[sat(25544, "ISS (ZARYA)")]);
        add_nuclear(&mut g, &[facility("Astravets", "Belarus")]);
        add_cyber(
            &mut g,
            &[
                indicator("domain", "evil-example.com", "botnet_cc"),
                indicator("ip:port", "192.0.2.44:8080", "botnet_cc"),
                indicator("url", "http://203.0.113.7/x.exe", "malware_download"),
            ],
            100,
        );
        g
    }

    #[test]
    fn flight_links_hex_callsign_registration() {
        let g = fixture_graph();
        let node = g.lookup("ae1450").expect("hex resolves");
        assert_eq!(node.kind, KIND_AIRCRAFT);
        let neighbors = g.neighbors(node);
        let rels: Vec<(&str, &str, &str)> = neighbors
            .iter()
            .map(|n| (n.entity.as_str(), n.kind.as_str(), n.relation.as_str()))
            .collect();
        assert!(rels.contains(&("TREK136", KIND_CALLSIGN, "callsign-of")));
        assert!(rels.contains(&("N123AB", KIND_AIRCRAFT, "registration-of")));
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let g = fixture_graph();
        assert!(g.lookup("AE1450").is_some());
        assert!(g.lookup("iss (zarya)").is_some());
        assert!(g.lookup("  25544 ").is_some());
    }

    #[test]
    fn satellite_name_and_norad_link_both_directions() {
        let g = fixture_graph();
        let norad = g.lookup("25544").expect("norad resolves");
        assert_eq!(norad.kind, KIND_NORAD_ID);
        let from_norad: Vec<_> = g
            .neighbors(norad)
            .iter()
            .map(|n| n.relation.clone())
            .collect();
        assert_eq!(from_norad, vec!["name-of".to_string()]);

        let name = g.lookup("ISS (ZARYA)").expect("name resolves");
        assert_eq!(name.kind, KIND_NAME);
        let from_name: Vec<_> = g
            .neighbors(name)
            .iter()
            .map(|n| n.relation.clone())
            .collect();
        // Inverse orientation: the norad id is "norad-id-of" the name.
        assert_eq!(from_name, vec!["norad-id-of".to_string()]);
    }

    #[test]
    fn nuclear_facility_links_country() {
        let g = fixture_graph();
        let f = g.lookup("Astravets").expect("facility resolves");
        assert_eq!(f.kind, KIND_FACILITY);
        let n = g.neighbors(f);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].entity, "Belarus");
        assert_eq!(n[0].kind, KIND_COUNTRY);
        assert_eq!(n[0].relation, "located-in");
        assert_eq!(n[0].source_feed, feeds::nuclear::FEED);
    }

    #[test]
    fn cyber_indicator_kinds_and_threat_links() {
        let g = fixture_graph();
        let d = g.lookup("evil-example.com").expect("domain resolves");
        assert_eq!(d.kind, KIND_DOMAIN);
        let ip = g.lookup("192.0.2.44").expect("ip resolves (port stripped)");
        assert_eq!(ip.kind, KIND_IP);
        let url_ip = g.lookup("203.0.113.7").expect("url host resolves");
        assert_eq!(url_ip.kind, KIND_IP);

        let tt = g.lookup("botnet_cc").expect("threat type resolves");
        assert_eq!(tt.kind, KIND_THREAT_TYPE);
        let n: Vec<String> = g.neighbors(tt).iter().map(|n| n.relation.clone()).collect();
        // From the threat-type side the relation reads "indicator-of".
        assert!(n.iter().all(|r| r == "indicator-of"));
        assert_eq!(n.len(), 2);
    }

    #[test]
    fn duplicate_links_dedupe() {
        let mut g = Graph::new();
        add_satellites(
            &mut g,
            &[sat(25544, "ISS (ZARYA)"), sat(25544, "ISS (ZARYA)")],
        );
        // n2yo catalog overlapping the TLE set adds no second edge either.
        add_catalog(
            &mut g,
            &[CatalogSat {
                name: "ISS (ZARYA)".to_string(),
                norad_id: 25544,
                latitude: None,
                longitude: None,
                altitude_km: None,
            }],
        );
        let node = g.lookup("25544").unwrap();
        assert_eq!(g.neighbors(node).len(), 1);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn bfs_depth_one_vs_two() {
        // Chain: 25544 -- name-of --> ISS (ZARYA). Extend the chain with a
        // second satellite sharing the same name string is unrealistic, so
        // build the chain via flights: hex -> callsign, plus a second
        // aircraft with the SAME callsign to make depth-2 reachable.
        let mut g = Graph::new();
        add_flights(
            &mut g,
            &[
                flight("aaaaaa", Some("SHARED1"), None),
                flight("bbbbbb", Some("SHARED1"), None),
            ],
        );
        let a = g.lookup("aaaaaa").unwrap();
        let d1 = g.expand(a, 1);
        let d1_vals: Vec<&str> = d1.nodes.iter().map(|n| n.value.as_str()).collect();
        assert!(d1_vals.contains(&"aaaaaa"));
        assert!(d1_vals.contains(&"SHARED1"));
        assert!(
            !d1_vals.contains(&"bbbbbb"),
            "depth 1 must not reach bbbbbb"
        );

        let d2 = g.expand(a, 2);
        let d2_vals: Vec<&str> = d2.nodes.iter().map(|n| n.value.as_str()).collect();
        assert!(d2_vals.contains(&"bbbbbb"), "depth 2 reaches bbbbbb");
        // Edges are the induced subgraph of visited nodes.
        assert!(d2.edges.iter().all(|e| {
            d2_vals.contains(&e.from.value.as_str()) && d2_vals.contains(&e.to.value.as_str())
        }));
    }

    #[test]
    fn bfs_depth_one_includes_origin_and_name() {
        let g = fixture_graph();
        let node = g.lookup("25544").unwrap();
        let e = g.expand(node, 1);
        assert!(e.nodes.iter().any(|n| n.value == "25544"));
        assert!(e.nodes.iter().any(|n| n.value == "ISS (ZARYA)"));
    }

    #[test]
    fn missing_entity_returns_none() {
        let g = fixture_graph();
        assert!(g.lookup("definitely-not-an-entity-xyz").is_none());
        assert!(g.lookup("").is_none());
        assert!(g.lookup("   ").is_none());
    }

    #[test]
    fn entity_value_trims_and_skips_empties() {
        let mut g = Graph::new();
        assert!(g.add_entity(KIND_AIRCRAFT, "   ").is_none());
        add_flights(&mut g, &[flight("  ae1450  ", None, None)]);
        // A bare hex with no links still doesn't create a node (no edges),
        // but an explicit add does.
        g.add_entity(KIND_AIRCRAFT, "ae1450");
        assert!(g.lookup("ae1450").is_some());
    }
}
