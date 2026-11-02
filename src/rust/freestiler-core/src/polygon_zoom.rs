//! Per-zoom polygon simplification and tile-tree clipping, opted into per
//! layer through [`crate::tiler::LayerData::polygon_simplify`].
//!
//! The default polygon path hands the whole polygon to a boolean-ops sweep
//! once for every tile in its bounding box. For one large polygon that cost
//! is (vertices x tiles): the HEC-RAS land-cover polygon that motivated this
//! module spent 99.9% of its z14-z17 time there, 75% of the total at the
//! maximum zoom, and most of those tiles turned out to be empty.
//!
//! An opted-in layer instead does, for each zoom:
//!
//! 1. **Simplify the whole polygon once for the zoom.** One Douglas-Peucker
//!    pass in Web Mercator ranks every vertex; a zoom keeps the vertices
//!    ranked above its tolerance, exactly what Douglas-Peucker at that
//!    tolerance would keep. Every tile of a zoom therefore cuts the same ring,
//!    so edges stay coincident across tile seams, and the error against the
//!    input is bounded by the tolerance with no accumulation over zooms. The
//!    layer's maximum zoom keeps every input vertex.
//! 2. **Cut it down a tile tree.** Each ring is clipped from a tile into its
//!    four children (Sutherland-Hodgman against the buffered tile), so a tile
//!    only sees its parent's fragment and a branch with nothing in it stops
//!    being visited.
//! 3. **Clean each fragment.** Sutherland-Hodgman joins the pieces of a
//!    concave ring with zero-width edges along the clip boundary, so each
//!    fragment goes through the same boolean-ops intersection as the default
//!    path, now on a few vertices rather than the whole ring.
//!
//! No feature is dropped. A ring always keeps at least three vertices, and a
//! feature left with no area anywhere at a zoom keeps its collapsed ring,
//! which the encoders write as a one-pixel square.
//!
//! Simplifying polygons independently moves vertices shared between them, so
//! this must not be used on a mesh, whose cells share edges.

use geo::BooleanOps;
use geo_types::{Coord, LineString, MultiPolygon, Polygon, Rect};
use rayon::prelude::*;
use std::collections::HashMap;

use crate::tiler::{Feature, Geometry, TileCoord};

/// Largest accepted tolerance, in tile pixels (1/4096 of a tile side).
pub const MAX_TOLERANCE_PX: f64 = 64.0;

/// Fragments below this many vertices are cut without forking rayon tasks.
const PARALLEL_VERTICES: usize = 4096;

/// Tile pixels per tile side, as the encoders use.
const EXTENT: f64 = 4096.0;

/// Web Mercator's latitude limit.
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

type Xy = [f64; 2];

/// A ring in normalized Web Mercator (0..1, y down), open, with the squared
/// distance above which each vertex survives Douglas-Peucker.
struct Ring {
    xy: Vec<Xy>,
    importance: Vec<f64>,
}

struct Part {
    exterior: Ring,
    holes: Vec<Ring>,
}

/// One feature's polygon parts, ranked for per-zoom simplification.
pub struct PreparedPolygon {
    parts: Vec<Part>,
    bounds: Option<[f64; 4]>,
}

/// A polygon part cut for a tile: plain rings, exterior first.
#[derive(Clone)]
struct Fragment {
    exterior: Vec<Xy>,
    holes: Vec<Vec<Xy>>,
}

/// True when `tolerance` is a usable per-layer tolerance in tile pixels.
pub fn valid_tolerance(tolerance: f64) -> bool {
    tolerance.is_finite() && tolerance > 0.0 && tolerance <= MAX_TOLERANCE_PX
}

fn to_mercator(c: &Coord<f64>) -> Xy {
    let lat = c.y.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    [
        (c.x + 180.0) / 360.0,
        0.5 - lat.tan().asinh() / (2.0 * std::f64::consts::PI),
    ]
}

fn to_lon_lat(p: &Xy) -> Coord<f64> {
    Coord {
        x: p[0] * 360.0 - 180.0,
        y: (std::f64::consts::PI * (1.0 - 2.0 * p[1]))
            .sinh()
            .atan()
            .to_degrees(),
    }
}

/// Squared distance from `p` to the segment `a`-`b`.
#[inline]
fn segment_distance2(p: &Xy, a: &Xy, b: &Xy) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (ex, ey) = (p[0] - (a[0] + t * dx), p[1] - (a[1] + t * dy));
    ex * ex + ey * ey
}

/// Rank a ring. The first vertex, the vertex farthest from it, and the vertex
/// farthest from their chord are always kept, so a ring never falls below
/// three vertices. Each other vertex gets the smaller of its own deviation and
/// its parent's rank, so keeping ranks above a tolerance reproduces
/// Douglas-Peucker at that tolerance on the two chains between the anchors.
fn prepare_ring(ring: &LineString<f64>) -> Ring {
    let mut xy: Vec<Xy> = Vec::with_capacity(ring.0.len());
    for c in &ring.0 {
        if !c.x.is_finite() || !c.y.is_finite() {
            continue;
        }
        let p = to_mercator(c);
        if xy.last() != Some(&p) {
            xy.push(p);
        }
    }
    while xy.len() > 1 && xy.first() == xy.last() {
        xy.pop();
    }
    let n = xy.len();
    let mut importance = vec![f64::INFINITY; n];
    if n < 4 {
        return Ring { xy, importance };
    }
    importance.iter_mut().for_each(|v| *v = 0.0);

    let vertex = |i: usize| &xy[if i == n { 0 } else { i }];
    let mut far = 0;
    let mut far_d = -1.0;
    for (i, p) in xy.iter().enumerate() {
        let d = (p[0] - xy[0][0]).powi(2) + (p[1] - xy[0][1]).powi(2);
        if d > far_d {
            far_d = d;
            far = i;
        }
    }
    importance[0] = f64::INFINITY;
    importance[far] = f64::INFINITY;

    // Sections run over index n, which stands for the closing vertex.
    let mut third: Option<(usize, f64)> = None;
    let mut stack: Vec<(usize, usize, f64)> = Vec::with_capacity(64);
    for (start, end) in [(0, far), (far, n)] {
        if end > start {
            stack.push((start, end, f64::INFINITY));
        }
    }
    while let Some((start, end, parent)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let (a, b) = (vertex(start), vertex(end));
        let (mut best, mut best_d) = (start, -1.0);
        for (i, p) in xy.iter().enumerate().take(end).skip(start + 1) {
            let d = segment_distance2(p, a, b);
            if d > best_d {
                best_d = d;
                best = i;
            }
        }
        if best_d <= 0.0 {
            continue; // Everything between lies on the chord: rank stays 0.
        }
        let rank = best_d.min(parent);
        importance[best] = rank;
        if parent.is_infinite() && third.map_or(true, |(_, d)| best_d > d) {
            third = Some((best, best_d));
        }
        stack.push((start, best, rank));
        stack.push((best, end, rank));
    }
    if let Some((i, _)) = third {
        importance[i] = f64::INFINITY;
    }
    Ring { xy, importance }
}

fn prepare_part(poly: &Polygon<f64>) -> Part {
    Part {
        exterior: prepare_ring(poly.exterior()),
        holes: poly.interiors().iter().map(prepare_ring).collect(),
    }
}

/// Rank a feature's polygon parts; `None` for any other geometry type.
pub fn prepare_feature(feature: &Feature) -> Option<PreparedPolygon> {
    let parts: Vec<Part> = match &feature.geometry {
        Geometry::Polygon(p) => vec![prepare_part(p)],
        Geometry::MultiPolygon(mp) => mp.0.iter().map(prepare_part).collect(),
        _ => return None,
    };
    let mut bounds: Option<[f64; 4]> = None;
    for p in parts.iter().flat_map(|part| &part.exterior.xy) {
        let b = bounds.get_or_insert([p[0], p[1], p[0], p[1]]);
        b[0] = b[0].min(p[0]);
        b[1] = b[1].min(p[1]);
        b[2] = b[2].max(p[0]);
        b[3] = b[3].max(p[1]);
    }
    Some(PreparedPolygon { parts, bounds })
}

fn simplified_ring(ring: &Ring, tolerance2: Option<f64>) -> Vec<Xy> {
    match tolerance2 {
        None => ring.xy.clone(),
        Some(t) => ring
            .xy
            .iter()
            .zip(&ring.importance)
            .filter(|(_, rank)| **rank > t)
            .map(|(p, _)| *p)
            .collect(),
    }
}

/// Clip a ring to `axis >= limit` (`keep_above`) or `axis <= limit`
/// (Sutherland-Hodgman). Crossing points are exactly on the limit.
fn clip_half(ring: &[Xy], axis: usize, limit: f64, keep_above: bool) -> Vec<Xy> {
    let inside = |p: &Xy| {
        if keep_above {
            p[axis] >= limit
        } else {
            p[axis] <= limit
        }
    };
    let mut out = Vec::with_capacity(ring.len() / 2 + 4);
    let Some(mut prev) = ring.last() else {
        return out;
    };
    let mut prev_in = inside(prev);
    for cur in ring {
        let cur_in = inside(cur);
        if cur_in != prev_in {
            let t = (limit - prev[axis]) / (cur[axis] - prev[axis]);
            let other = 1 - axis;
            let mut crossing = [0.0; 2];
            crossing[axis] = limit;
            crossing[other] = prev[other] + t * (cur[other] - prev[other]);
            out.push(crossing);
        }
        if cur_in {
            out.push(*cur);
        }
        prev = cur;
        prev_in = cur_in;
    }
    out
}

/// Clip a ring to a band; a ring already inside it is only copied.
fn clip_band(ring: &[Xy], axis: usize, low: f64, high: f64) -> Vec<Xy> {
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in ring {
        min = min.min(p[axis]);
        max = max.max(p[axis]);
    }
    if max < low || min > high {
        return Vec::new();
    }
    match (min < low, max > high) {
        (false, false) => ring.to_vec(),
        (true, false) => clip_half(ring, axis, low, true),
        (false, true) => clip_half(ring, axis, high, false),
        (true, true) => clip_half(&clip_half(ring, axis, low, true), axis, high, false),
    }
}

/// Clip fragments to a band. A fragment whose exterior leaves the band goes,
/// holes included.
fn clip_fragments(fragments: &[Fragment], axis: usize, low: f64, high: f64) -> Vec<Fragment> {
    fragments
        .iter()
        .filter_map(|f| {
            let exterior = clip_band(&f.exterior, axis, low, high);
            if exterior.is_empty() {
                return None;
            }
            let holes = f
                .holes
                .iter()
                .map(|h| clip_band(h, axis, low, high))
                .filter(|h| !h.is_empty())
                .collect();
            Some(Fragment { exterior, holes })
        })
        .collect()
}

fn vertex_count(fragments: &[Fragment]) -> usize {
    fragments
        .iter()
        .map(|f| f.exterior.len() + f.holes.iter().map(Vec::len).sum::<usize>())
        .sum()
}

/// Tile address in a tile tree.
type TileAddress = (u32, u32, u8);

/// Cut `fragments`, which lie inside `tile`, down to zoom `target`. Leaves come
/// back in a fixed order whatever the thread count.
fn descend(
    fragments: Vec<Fragment>,
    tile: TileAddress,
    target: u8,
    buffer: f64,
) -> Vec<(u32, u32, Vec<Fragment>)> {
    let (x, y, level) = tile;
    if fragments.is_empty() {
        return Vec::new();
    }
    if level == target {
        return vec![(x, y, fragments)];
    }
    let scale = (1u64 << (level + 1)) as f64;
    let margin = buffer / scale;
    let edge = |i: u32| i as f64 / scale;
    let (cx, cy) = (x * 2, y * 2);
    let parallel = vertex_count(&fragments) >= PARALLEL_VERTICES;
    let child = |column: u32, row: u32, band: &[Fragment]| {
        let cut = clip_fragments(
            band,
            1,
            edge(cy + row) - margin,
            edge(cy + row + 1) + margin,
        );
        descend(cut, (cx + column, cy + row, level + 1), target, buffer)
    };
    let column = |column: u32| {
        let band = clip_fragments(
            &fragments,
            0,
            edge(cx + column) - margin,
            edge(cx + column + 1) + margin,
        );
        if parallel {
            let (mut top, bottom) =
                rayon::join(|| child(column, 0, &band), || child(column, 1, &band));
            top.extend(bottom);
            top
        } else {
            let mut top = child(column, 0, &band);
            top.extend(child(column, 1, &band));
            top
        }
    };
    if parallel {
        let (mut west, east) = rayon::join(|| column(0), || column(1));
        west.extend(east);
        west
    } else {
        let mut west = column(0);
        west.extend(column(1));
        west
    }
}

/// The deepest tile at or above `target` holding every tile of `target`
/// whose buffered rectangle can touch `bounds`.
fn seed_tile(bounds: &[f64; 4], target: u8, buffer: f64) -> TileAddress {
    let margin = buffer / (1u64 << target) as f64;
    for level in (1..=target).rev() {
        let n = (1u64 << level) as f64;
        let index = |v: f64| ((v * n).floor().clamp(0.0, n - 1.0)) as u32;
        let (x0, x1) = (index(bounds[0] - margin), index(bounds[2] + margin));
        let (y0, y1) = (index(bounds[1] - margin), index(bounds[3] + margin));
        if x0 == x1 && y0 == y1 {
            return (x0, y0, level);
        }
    }
    (0, 0, 0)
}

fn lon_lat_ring(ring: &[Xy]) -> LineString<f64> {
    let mut coords: Vec<Coord<f64>> = ring.iter().map(to_lon_lat).collect();
    if let Some(first) = coords.first().copied() {
        coords.push(first);
    }
    LineString(coords)
}

/// The buffered tile rectangle in longitude and latitude.
fn lon_lat_rect(x: u32, y: u32, z: u8, buffer: f64) -> Rect<f64> {
    let n = (1u64 << z) as f64;
    let margin = buffer / n;
    let min = to_lon_lat(&[x as f64 / n - margin, (y + 1) as f64 / n + margin]);
    let max = to_lon_lat(&[(x + 1) as f64 / n + margin, y as f64 / n - margin]);
    Rect::new(min, max)
}

fn touches_edge(ring: &[Xy], x: u32, y: u32, z: u8, buffer: f64) -> bool {
    let n = (1u64 << z) as f64;
    let margin = buffer / n;
    let (x0, x1) = (x as f64 / n - margin, (x + 1) as f64 / n + margin);
    let (y0, y1) = (y as f64 / n - margin, (y + 1) as f64 / n + margin);
    ring.iter()
        .any(|p| p[0] <= x0 || p[0] >= x1 || p[1] <= y0 || p[1] >= y1)
}

/// Turn a tile's fragments into a clean geometry, or `None` when nothing with
/// area is left (a tile lying wholly inside a hole, a sliver in the buffer).
fn clean_leaf(x: u32, y: u32, z: u8, buffer: f64, fragments: &[Fragment]) -> Option<Geometry> {
    let clip = lon_lat_rect(x, y, z, buffer).to_polygon();
    let mut polygons: Vec<Polygon<f64>> = Vec::new();
    for f in fragments {
        let holes = f.holes.iter().map(|h| lon_lat_ring(h)).collect();
        let polygon = Polygon::new(lon_lat_ring(&f.exterior), holes);
        let cleaned = polygon.intersection(&clip);
        if !cleaned.0.is_empty() {
            polygons.extend(cleaned.0);
        } else if f.holes.is_empty() && !touches_edge(&f.exterior, x, y, z, buffer) {
            // Collapsed below the overlay's precision, inside the tile: the
            // encoders write it as a one-pixel square.
            polygons.push(polygon);
        }
    }
    match polygons.len() {
        0 => None,
        1 => polygons.pop().map(Geometry::Polygon),
        _ => Some(Geometry::MultiPolygon(MultiPolygon(polygons))),
    }
}

/// One clipped piece of a feature: tile column, tile row, geometry.
type Piece = (u32, u32, Geometry);

/// Simplified and clipped pieces of one feature at one zoom, with the number
/// of vertices kept before clipping.
fn tile_feature(
    prepared: &PreparedPolygon,
    zoom: u8,
    tolerance2: Option<f64>,
    buffer: f64,
) -> (Vec<Piece>, usize) {
    let Some(bounds) = &prepared.bounds else {
        return (Vec::new(), 0);
    };
    let fragments: Vec<Fragment> = prepared
        .parts
        .iter()
        .map(|part| Fragment {
            exterior: simplified_ring(&part.exterior, tolerance2),
            holes: part
                .holes
                .iter()
                .map(|h| simplified_ring(h, tolerance2))
                .filter(|h| !h.is_empty())
                .collect(),
        })
        .filter(|f| !f.exterior.is_empty())
        .collect();
    let kept = vertex_count(&fragments);
    let seed = seed_tile(bounds, zoom, buffer);
    let leaves = descend(fragments, seed, zoom, buffer);
    let cleaned: Vec<Option<Geometry>> = leaves
        .par_iter()
        .map(|(x, y, leaf)| clean_leaf(*x, *y, zoom, buffer, leaf))
        .collect();
    let mut out: Vec<Piece> = leaves
        .iter()
        .zip(cleaned)
        .filter_map(|((x, y, _), g)| g.map(|g| (*x, *y, g)))
        .collect();
    if out.is_empty() {
        // Nothing with area anywhere at this zoom: the feature stays, as its
        // collapsed ring in the first tile it reaches.
        if let Some((x, y, leaf)) = leaves.first() {
            let ring = lon_lat_ring(&leaf[0].exterior);
            out.push((*x, *y, Geometry::Polygon(Polygon::new(ring, Vec::new()))));
        }
    }
    (out, kept)
}

/// Pieces of an opted-in layer at one zoom.
pub struct ZoomPolygons {
    /// Feature index and clipped geometry per tile, in feature order.
    pub tiles: HashMap<TileCoord, Vec<(usize, Geometry)>>,
    /// Vertices kept at this zoom, before clipping.
    pub vertices: usize,
}

/// Simplify and clip an opted-in layer's polygons for one zoom. `tolerance_px`
/// is `None` at the layer's maximum zoom, which keeps every vertex.
pub fn tile_zoom(
    prepared: &[Option<PreparedPolygon>],
    keep: Option<&[bool]>,
    zoom: u8,
    tolerance_px: Option<f64>,
    buffer: f64,
) -> ZoomPolygons {
    let tolerance2 = tolerance_px.map(|px| {
        let tolerance = px / (EXTENT * (1u64 << zoom) as f64);
        tolerance * tolerance
    });
    let per_feature: Vec<(usize, Vec<Piece>, usize)> = prepared
        .par_iter()
        .enumerate()
        .filter(|(i, _)| keep.map_or(true, |k| k[*i]))
        .filter_map(|(i, p)| p.as_ref().map(|p| (i, p)))
        .map(|(i, p)| {
            let (pieces, kept) = tile_feature(p, zoom, tolerance2, buffer);
            (i, pieces, kept)
        })
        .collect();
    let mut tiles: HashMap<TileCoord, Vec<(usize, Geometry)>> = HashMap::new();
    let mut vertices = 0;
    for (i, pieces, kept) in per_feature {
        vertices += kept;
        for (x, y, geometry) in pieces {
            tiles
                .entry(TileCoord { z: zoom, x, y })
                .or_default()
                .push((i, geometry));
        }
    }
    ZoomPolygons { tiles, vertices }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_ring(x: f64, y: f64, half: f64, per_side: usize) -> Vec<Xy> {
        let mut out = Vec::new();
        let corners = [
            [x - half, y - half],
            [x + half, y - half],
            [x + half, y + half],
            [x - half, y + half],
        ];
        for c in 0..4 {
            let (a, b) = (corners[c], corners[(c + 1) % 4]);
            for i in 0..per_side {
                let t = i as f64 / per_side as f64;
                out.push([a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]);
            }
        }
        out
    }

    fn area(ring: &[Xy]) -> f64 {
        let n = ring.len();
        (0..n)
            .map(|i| {
                let (a, b) = (ring[i], ring[(i + 1) % n]);
                a[0] * b[1] - b[0] * a[1]
            })
            .sum::<f64>()
            .abs()
            / 2.0
    }

    #[test]
    fn clipping_keeps_a_surrounding_ring_as_the_rectangle() {
        let ring = square_ring(0.5, 0.5, 0.4, 3);
        let band = clip_band(&ring, 0, 0.45, 0.55);
        let cell = clip_band(&band, 1, 0.45, 0.55);
        assert!((area(&cell) - 0.01).abs() < 1e-12);
    }

    #[test]
    fn clipping_a_ring_outside_gives_nothing() {
        let ring = square_ring(0.2, 0.2, 0.1, 3);
        assert!(clip_band(&ring, 0, 0.5, 0.6).is_empty());
    }

    #[test]
    fn descent_covers_the_polygon_without_gaps() {
        let ring = square_ring(0.5, 0.5, 0.3, 50);
        let whole = area(&ring);
        let fragments = vec![Fragment {
            exterior: ring,
            holes: Vec::new(),
        }];
        let leaves = descend(fragments, (0, 0, 0), 5, 0.0);
        let sum: f64 = leaves
            .iter()
            .flat_map(|(_, _, f)| f)
            .map(|f| area(&f.exterior))
            .sum();
        assert!((sum - whole).abs() < 1e-9, "{sum} vs {whole}");
    }

    #[test]
    fn ranks_reproduce_douglas_peucker_and_keep_a_triangle() {
        let coords: Vec<Coord<f64>> = (0..=200)
            .map(|i| {
                let a = i as f64 / 200.0 * std::f64::consts::TAU;
                Coord {
                    x: 10.0 * a.cos() + 0.01 * (17.0 * a).sin(),
                    y: 10.0 * a.sin(),
                }
            })
            .collect();
        let ring = prepare_ring(&LineString(coords));
        // A tolerance far above the ring's size still leaves a triangle.
        let triangle = simplified_ring(&ring, Some(1.0));
        assert_eq!(triangle.len(), 3);
        // Every vertex dropped at a tolerance lies within it of the result.
        for tolerance in [1e-3f64, 1e-4, 1e-5] {
            let kept = simplified_ring(&ring, Some(tolerance * tolerance));
            assert!(kept.len() >= 3);
            for p in &ring.xy {
                let d = (0..kept.len())
                    .map(|i| segment_distance2(p, &kept[i], &kept[(i + 1) % kept.len()]))
                    .fold(f64::INFINITY, f64::min);
                assert!(d <= tolerance * tolerance * (1.0 + 1e-9));
            }
        }
        // Without a tolerance nothing goes.
        assert_eq!(simplified_ring(&ring, None).len(), ring.xy.len());
    }

    #[test]
    fn seed_contains_every_tile_the_buffer_can_reach() {
        // Just west of a z1 boundary: the z8 buffer crosses into the east half.
        let bounds = [0.5 - 1e-6, 0.26, 0.5 - 1e-7, 0.27];
        assert_eq!(seed_tile(&bounds, 8, 0.02), (0, 0, 0));
        // Well inside one z4 tile.
        let bounds = [0.53, 0.53, 0.54, 0.54];
        let (x, y, level) = seed_tile(&bounds, 8, 0.02);
        assert!(level >= 4);
        let n = (1u64 << level) as f64;
        assert!(x as f64 / n <= 0.53 && (x + 1) as f64 / n >= 0.54);
        assert!(y as f64 / n <= 0.53 && (y + 1) as f64 / n >= 0.54);
    }
}
