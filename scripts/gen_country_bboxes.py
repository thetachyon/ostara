#!/usr/bin/env python3
"""Generate data/country_bboxes.json for the country-inspect resolver.

Downloads the public-domain 110m countries GeoJSON (datasets/geo-countries,
PDDL 1.0) and computes per-country {name, iso2, iso3, bbox, centroid}.
Run once; the JSON is committed and embedded via include_str!.

Usage: python3 scripts/gen_country_bboxes.py
"""
import json
import sys
import urllib.request
from datetime import date

SRC_URL = "https://raw.githubusercontent.com/datasets/geo-countries/master/data/countries.geojson"
OUT_PATH = "data/country_bboxes.json"


def poly_bbox(rings):
    xs = [p[0] for ring in rings for p in ring]
    ys = [p[1] for ring in rings for p in ring]
    return min(xs), min(ys), max(xs), max(ys)


def bbox_area(w, s, e, n):
    width = (e - w) if w <= e else (180.0 - w) + (e + 180.0)
    return width * (n - s)


def walk_coords(geom):
    t = geom["type"]
    coords = geom["coordinates"]
    if t == "Polygon":
        for ring in coords:
            yield from ring
    elif t == "MultiPolygon":
        for poly in coords:
            for ring in poly:
                yield from ring
    else:
        raise ValueError(f"unexpected geometry type {t}")


def metro_center(geom):
    """Center of the largest polygon's bbox — a metropolitan-territory proxy.

    Coordinate-mean centroids are dragged by overseas territories (France's
    mean centroid lands in the Atlantic); the largest polygon is the main
    territory for the ocean fallback.
    """
    t = geom["type"]
    coords = geom["coordinates"]
    polys = [coords] if t == "Polygon" else list(coords)
    best = None
    for poly in polys:
        w, s, e, n = poly_bbox(poly)
        a = bbox_area(w, s, e, n)
        if best is None or a > best[0]:
            best = (a, w, s, e, n)
    _, w, s, e, n = best
    return _metro(w, s, e, n)


def _metro(w, s, e, n):
    cx = (w + e) / 2.0 if w <= e else (w + e + 360.0) / 2.0 - 180.0
    if cx > 180.0:
        cx -= 360.0
    return [round(cx, 3), round((s + n) / 2.0, 3)]


def polygon_bboxes(geom):
    """One bbox per polygon. A single full-geometry bbox is broken-wide for
    countries with far-flung territories (NZL's spans 357° of longitude,
    making the South Atlantic 'New Zealand'); per-polygon bboxes keep the
    metropolitan bbox tight while still resolving clicks on overseas
    islands to the right country."""
    t = geom["type"]
    coords = geom["coordinates"]
    polys = [coords] if t == "Polygon" else list(coords)
    out = []
    for poly in polys:
        w, s, e, n = poly_bbox(poly)
        out.append([round(w, 3), round(s, 3), round(e, 3), round(n, 3)])
    return out


def main():
    print(f"downloading {SRC_URL} ...")
    with urllib.request.urlopen(SRC_URL, timeout=60) as r:
        fc = json.load(r)
    feats = fc["features"]
    print(f"{len(feats)} features")

    countries = []
    for f in feats:
        p = f.get("properties", {}) or {}
        name = p.get("name") or p.get("NAME") or ""
        iso3 = (p.get("ISO3166-1-Alpha-3") or "").upper()
        iso2 = (p.get("ISO3166-1-Alpha-2") or "").upper()
        if not name or not iso3:
            print(f"  SKIP feature with name={name!r} iso3={iso3!r}", file=sys.stderr)
            continue
        countries.append({
            "name": name,
            "iso2": iso2,
            "iso3": iso3,
            "metro": metro_center(f["geometry"]),
            "bboxes": polygon_bboxes(f["geometry"]),
        })

    countries.sort(key=lambda c: c["iso3"])
    out = {
        "meta": {
            "source": SRC_URL,
            "license": "PDDL 1.0 (public-domain dedication)",
            "generated": date.today().isoformat(),
            "method": "one bbox per polygon per feature (a single full-geometry bbox is broken-wide for far-flung territories — NZL's spans 357° of longitude), plus metro center (center of the largest polygon's bbox) for the ocean fallback",
            "count": len(countries),
        },
        "countries": countries,
    }
    with open(OUT_PATH, "w") as fh:
        json.dump(out, fh, separators=(",", ":"))
    import os
    print(f"wrote {OUT_PATH}: {os.path.getsize(OUT_PATH)} bytes, {len(countries)} countries")


if __name__ == "__main__":
    main()
