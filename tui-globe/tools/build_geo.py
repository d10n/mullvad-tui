#!/usr/bin/env python3

# SPDX-License-Identifier: GPL-3.0-or-later

"""Generate tui-globe's `.gl` geometry buffers from Natural Earth data.

The buffers are little-endian binary blobs in the same format the desktop
Mullvad client uploads into WebGL:

  land_positions.gl         f32  xyz triplets on the unit sphere
  land_triangle_indices.gl  u32  GL_TRIANGLES indices
  land_contour_indices.gl   u32  GL_LINE_STRIP indices, 0xFFFFFFFF between rings
  ocean_positions.gl        f32  icosphere xyz triplets
  ocean_indices.gl          u32  icosphere GL_TRIANGLES indices

Pick a Natural Earth scale via --scale (10m | 50m | 110m). With cultural
borders enabled (the default), land triangulation comes from
`ne_<scale>_admin_0_countries` so the contour buffer naturally contains
coastlines plus every inland country border, and `ne_<scale>_admin_1_states_
provinces_lines` is overlaid as additional `LINE_STRIP` rings. With
`--no-cultural-borders`, triangulation falls back to the physical-only
`ne_<scale>_land` shapefile (coastlines only, smaller binary).

Shapefiles are downloaded from naciscdn.org on demand and cached under
`--data-dir` (default `$XDG_CACHE_HOME/tui-globe-build-geo`).

Coordinate convention matches `tui-globe/src/lib.rs` `project_point`:
  x = cos(lat) * sin(lon)
  y = sin(lat)
  z = cos(lat) * cos(lon)

Usage:
  python3 build_geo.py --scale 50m --out ../assets/geo
  python3 build_geo.py --scale 10m --no-cultural-borders --out ../assets/geo
"""

from __future__ import annotations

import argparse
import math
import os
import sys
import urllib.request
import zipfile
from pathlib import Path

import mapbox_earcut as earcut
import numpy as np
import shapefile  # pyshp


RESTART = 0xFFFFFFFF
MAX_STEP_DEG = 0.5
ICOSPHERE_SUBDIVISIONS = 4
NE_BASE_URL = "https://naciscdn.org/naturalearth"
SCALES = ("10m", "50m", "110m")


def default_cache_dir() -> Path:
    base = os.environ.get("XDG_CACHE_HOME") or str(Path.home() / ".cache")
    return Path(base) / "tui-globe-build-geo"


def fetch_shapefile(scale: str, category: str, name: str, cache_dir: Path) -> Path:
    """Download (if needed), unzip, and return the path to a Natural Earth `.shp`.

    `category` is `"physical"` or `"cultural"`. Files are cached by name; an
    existing extracted `.shp` short-circuits both download and unzip.
    """
    cache_dir.mkdir(parents=True, exist_ok=True)
    shp_path = cache_dir / f"{name}.shp"
    if shp_path.exists():
        return shp_path
    zip_path = cache_dir / f"{name}.zip"
    if not zip_path.exists():
        url = f"{NE_BASE_URL}/{scale}/{category}/{name}.zip"
        print(f"Downloading {url}")
        urllib.request.urlretrieve(url, zip_path)
    with zipfile.ZipFile(zip_path) as z:
        z.extractall(cache_dir)
    return shp_path


def signed_area(ring: list[tuple[float, float]]) -> float:
    s = 0.0
    n = len(ring)
    for i in range(n):
        x1, y1 = ring[i]
        x2, y2 = ring[(i + 1) % n]
        s += (x2 - x1) * (y2 + y1)
    return s


def densify_closed(ring: list[tuple[float, float]], max_step_deg: float) -> list[tuple[float, float]]:
    """Subdivide a closed ring's edges so no segment exceeds `max_step_deg`."""
    out: list[tuple[float, float]] = []
    n = len(ring)
    for i in range(n):
        x1, y1 = ring[i]
        x2, y2 = ring[(i + 1) % n]
        out.append((x1, y1))
        for p in _interior_points((x1, y1), (x2, y2), max_step_deg):
            out.append(p)
    return out


def densify_open(line: list[tuple[float, float]], max_step_deg: float) -> list[tuple[float, float]]:
    """Subdivide an open polyline's edges so no segment exceeds `max_step_deg`."""
    out: list[tuple[float, float]] = []
    for i in range(len(line) - 1):
        a, b = line[i], line[i + 1]
        out.append(a)
        for p in _interior_points(a, b, max_step_deg):
            out.append(p)
    if line:
        out.append(line[-1])
    return out


def _interior_points(
    a: tuple[float, float], b: tuple[float, float], max_step_deg: float
) -> list[tuple[float, float]]:
    d = max(abs(b[0] - a[0]), abs(b[1] - a[1]))
    steps = int(math.ceil(d / max_step_deg))
    if steps <= 1:
        return []
    return [
        (a[0] + (b[0] - a[0]) * (k / steps), a[1] + (b[1] - a[1]) * (k / steps))
        for k in range(1, steps)
    ]


def project(lon_deg: float, lat_deg: float) -> tuple[float, float, float]:
    lon = math.radians(lon_deg)
    lat = math.radians(lat_deg)
    cl = math.cos(lat)
    return (cl * math.sin(lon), math.sin(lat), cl * math.cos(lon))


def split_into_polygons(shape: shapefile.Shape) -> list[list[list[tuple[float, float]]]]:
    """Group a polygon shape's rings into [outer, hole, hole, ...] lists.

    Shapefile convention (ESRI): clockwise rings are outer boundaries,
    counter-clockwise rings are holes. `signed_area` returns positive for
    clockwise, so sign > 0 starts a new outer; negative rings attach as
    holes to the most recent outer.
    """
    parts = list(shape.parts) + [len(shape.points)]
    rings: list[list[tuple[float, float]]] = []
    for i in range(len(parts) - 1):
        ring = [(p[0], p[1]) for p in shape.points[parts[i]:parts[i + 1]]]
        if len(ring) >= 2 and ring[0] == ring[-1]:
            ring = ring[:-1]
        if len(ring) >= 3:
            rings.append(ring)

    polys: list[list[list[tuple[float, float]]]] = []
    for ring in rings:
        if signed_area(ring) > 0:
            polys.append([ring])
        else:
            if not polys:
                polys.append([list(reversed(ring))])
            else:
                polys[-1].append(ring)
    return polys


def split_into_lines(shape: shapefile.Shape) -> list[list[tuple[float, float]]]:
    """Split a polyline shape into one list of (lon, lat) tuples per part."""
    parts = list(shape.parts) + [len(shape.points)]
    lines: list[list[tuple[float, float]]] = []
    for i in range(len(parts) - 1):
        line = [(p[0], p[1]) for p in shape.points[parts[i]:parts[i + 1]]]
        if len(line) >= 2:
            lines.append(line)
    return lines


class MeshBuilder:
    """Accumulates positions + triangle indices + LINE_STRIP contour indices."""

    def __init__(self) -> None:
        self.positions: list[float] = []
        self.triangle_indices: list[int] = []
        self.contour_indices: list[int] = []

    def _append_vertex(self, lon: float, lat: float) -> int:
        idx = len(self.positions) // 3
        x, y, z = project(lon, lat)
        self.positions.extend((x, y, z))
        return idx

    def add_polygon(self, rings: list[list[tuple[float, float]]]) -> None:
        """Triangulate a polygon (one outer ring + zero or more holes) and
        emit each ring as a closed LINE_STRIP into the contour buffer."""
        densified = [densify_closed(r, MAX_STEP_DEG) for r in rings]

        flat: list[float] = []
        ring_bases: list[int] = []
        ring_lengths: list[int] = []
        for ring in densified:
            ring_bases.append(len(self.positions) // 3)
            ring_lengths.append(len(ring))
            for lon, lat in ring:
                self._append_vertex(lon, lat)
                flat.extend((lon, lat))

        verts = np.asarray(flat, dtype=np.float64).reshape(-1, 2)
        ring_ends = []
        cumulative = 0
        for length in ring_lengths:
            cumulative += length
            ring_ends.append(cumulative)
        local_tris = earcut.triangulate_float64(verts, np.asarray(ring_ends, dtype=np.uint32))

        offset = ring_bases[0]
        for local_idx in local_tris:
            self.triangle_indices.append(int(local_idx) + offset)

        for base, length in zip(ring_bases, ring_lengths):
            for k in range(length):
                self.contour_indices.append(base + k)
            self.contour_indices.append(base)  # close the ring
            self.contour_indices.append(RESTART)

    def add_polyline(self, line: list[tuple[float, float]]) -> None:
        """Append a non-closed polyline as one LINE_STRIP (no triangles)."""
        densified = densify_open(line, MAX_STEP_DEG)
        if len(densified) < 2:
            return
        base = len(self.positions) // 3
        for lon, lat in densified:
            self._append_vertex(lon, lat)
        for k in range(len(densified)):
            self.contour_indices.append(base + k)
        self.contour_indices.append(RESTART)

    def finish(self) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
        contours = list(self.contour_indices)
        while contours and contours[-1] == RESTART:
            contours.pop()
        return (
            np.asarray(self.positions, dtype=np.float32),
            np.asarray(self.triangle_indices, dtype=np.uint32),
            np.asarray(contours, dtype=np.uint32),
        )


def build_icosphere(subdivisions: int) -> tuple[np.ndarray, np.ndarray]:
    t = (1.0 + math.sqrt(5.0)) / 2.0
    base_verts = [
        (-1, t, 0), (1, t, 0), (-1, -t, 0), (1, -t, 0),
        (0, -1, t), (0, 1, t), (0, -1, -t), (0, 1, -t),
        (t, 0, -1), (t, 0, 1), (-t, 0, -1), (-t, 0, 1),
    ]
    verts = [normalize(v) for v in base_verts]
    faces = [
        (0, 11, 5), (0, 5, 1), (0, 1, 7), (0, 7, 10), (0, 10, 11),
        (1, 5, 9), (5, 11, 4), (11, 10, 2), (10, 7, 6), (7, 1, 8),
        (3, 9, 4), (3, 4, 2), (3, 2, 6), (3, 6, 8), (3, 8, 9),
        (4, 9, 5), (2, 4, 11), (6, 2, 10), (8, 6, 7), (9, 8, 1),
    ]

    midpoint_cache: dict[tuple[int, int], int] = {}

    def midpoint(a: int, b: int) -> int:
        key = (min(a, b), max(a, b))
        if key in midpoint_cache:
            return midpoint_cache[key]
        va, vb = verts[a], verts[b]
        m = normalize(((va[0] + vb[0]) / 2, (va[1] + vb[1]) / 2, (va[2] + vb[2]) / 2))
        verts.append(m)
        idx = len(verts) - 1
        midpoint_cache[key] = idx
        return idx

    for _ in range(subdivisions):
        new_faces: list[tuple[int, int, int]] = []
        for a, b, c in faces:
            ab = midpoint(a, b)
            bc = midpoint(b, c)
            ca = midpoint(c, a)
            new_faces.extend([(a, ab, ca), (b, bc, ab), (c, ca, bc), (ab, bc, ca)])
        faces = new_faces

    positions = np.asarray([c for v in verts for c in v], dtype=np.float32)
    indices = np.asarray([i for f in faces for i in f], dtype=np.uint32)
    return positions, indices


def normalize(v: tuple[float, float, float]) -> tuple[float, float, float]:
    n = math.sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2])
    return (v[0] / n, v[1] / n, v[2] / n)


def write_le(path: Path, arr: np.ndarray) -> None:
    if arr.dtype not in (np.float32, np.uint32):
        raise ValueError(f"unsupported dtype: {arr.dtype}")
    arr_le = arr.astype(arr.dtype.newbyteorder("<"), copy=False)
    path.write_bytes(arr_le.tobytes())
    print(f"  wrote {path.name:30s} {arr.size:>9d} elements ({path.stat().st_size} bytes)")


def main() -> int:
    ap = argparse.ArgumentParser(
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
        description=__doc__,
    )
    ap.add_argument(
        "--scale",
        choices=SCALES,
        default="50m",
        help="Natural Earth scale to fetch",
    )
    ap.add_argument(
        "--cultural-borders",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="include country + state/province borders (admin_0_countries + admin_1_lines); "
        "with --no-cultural-borders, use ne_<scale>_land for coastlines only",
    )
    ap.add_argument(
        "--data-dir",
        type=Path,
        default=default_cache_dir(),
        help="directory to cache downloaded shapefiles",
    )
    ap.add_argument(
        "--ne-countries",
        type=Path,
        default=None,
        help="override: path to a custom admin_0_countries .shp (used with cultural borders)",
    )
    ap.add_argument(
        "--ne-states",
        type=Path,
        default=None,
        help="override: path to a custom admin_1_states_provinces_lines .shp",
    )
    ap.add_argument(
        "--ne-land",
        type=Path,
        default=None,
        help="override: path to a custom physical land .shp (used with --no-cultural-borders)",
    )
    ap.add_argument("--out", type=Path, required=True, help="output directory")
    args = ap.parse_args()

    if args.cultural_borders and args.ne_land is not None:
        ap.error("--ne-land is only meaningful with --no-cultural-borders")
    if not args.cultural_borders and (args.ne_countries is not None or args.ne_states is not None):
        ap.error("--ne-countries / --ne-states require cultural borders to be enabled")

    args.out.mkdir(parents=True, exist_ok=True)

    builder = MeshBuilder()

    if args.cultural_borders:
        countries_shp = args.ne_countries or fetch_shapefile(
            args.scale, "cultural", f"ne_{args.scale}_admin_0_countries", args.data_dir
        )
        states_shp = args.ne_states or fetch_shapefile(
            args.scale,
            "cultural",
            f"ne_{args.scale}_admin_1_states_provinces_lines",
            args.data_dir,
        )
        print("Adding country polygons from", countries_shp)
        with shapefile.Reader(str(countries_shp)) as reader:
            for shape in reader.shapes():
                for poly in split_into_polygons(shape):
                    builder.add_polygon(poly)
        print("Adding state/province lines from", states_shp)
        with shapefile.Reader(str(states_shp)) as reader:
            for shape in reader.shapes():
                for line in split_into_lines(shape):
                    builder.add_polyline(line)
    else:
        land_shp = args.ne_land or fetch_shapefile(
            args.scale, "physical", f"ne_{args.scale}_land", args.data_dir
        )
        print("Adding land polygons from", land_shp)
        with shapefile.Reader(str(land_shp)) as reader:
            for shape in reader.shapes():
                for poly in split_into_polygons(shape):
                    builder.add_polygon(poly)

    land_pos, land_tris, land_contours = builder.finish()
    write_le(args.out / "land_positions.gl", land_pos)
    write_le(args.out / "land_triangle_indices.gl", land_tris)
    write_le(args.out / "land_contour_indices.gl", land_contours)

    print(f"Building level-{ICOSPHERE_SUBDIVISIONS} icosphere ocean mesh")
    ocean_pos, ocean_idx = build_icosphere(ICOSPHERE_SUBDIVISIONS)
    write_le(args.out / "ocean_positions.gl", ocean_pos)
    write_le(args.out / "ocean_indices.gl", ocean_idx)

    return 0


if __name__ == "__main__":
    sys.exit(main())
