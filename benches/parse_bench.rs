//! Criterion benchmarks for the normalization hot paths.
//!
//! These measure pure parse+normalize work on real upstream payloads saved
//! under `benches/fixtures/` — no networking. The numbers land in the README
//! benchmark table.

use criterion::{Criterion, Throughput, black_box, criterion_group, criterion_main};
use ostara::feeds::{flights, news, quakes, satellites};

fn bench_usgs(c: &mut Criterion) {
    let body = include_str!("fixtures/usgs.json");
    let mut group = c.benchmark_group("usgs_geojson_parse");
    group.throughput(Throughput::Bytes(body.len() as u64));
    group.bench_function("parse_11kb", |b| {
        b.iter(|| quakes::parse_feed(black_box(body)).unwrap())
    });
    group.finish();
}

fn bench_adsb(c: &mut Criterion) {
    let body = include_str!("fixtures/adsb_mil.json");
    let mut group = c.benchmark_group("adsb_json_parse");
    group.throughput(Throughput::Bytes(body.len() as u64));
    group.bench_function("parse_41kb_111_aircraft", |b| {
        b.iter(|| flights::parse_response(black_box(body), "mil", 1000).unwrap())
    });
    group.finish();
}

fn bench_tle(c: &mut Criterion) {
    let body = include_str!("fixtures/celestrak.tle");
    let mut group = c.benchmark_group("tle_parse");
    group.throughput(Throughput::Bytes(body.len() as u64));
    group.bench_function("parse_16_objects", |b| {
        b.iter(|| satellites::parse_tle(black_box(body)))
    });
    group.finish();
}

fn bench_gdelt(c: &mut Criterion) {
    let body = include_str!("fixtures/gdelt.json");
    let mut group = c.benchmark_group("gdelt_json_parse");
    group.throughput(Throughput::Bytes(body.len() as u64));
    group.bench_function("parse_artlist", |b| {
        b.iter(|| news::parse_response(black_box(body), "earthquake").unwrap())
    });
    group.finish();
}

criterion_group!(benches, bench_usgs, bench_adsb, bench_tle, bench_gdelt);
criterion_main!(benches);
