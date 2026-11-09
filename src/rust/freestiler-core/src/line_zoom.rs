//! Per-zoom line simplification, opted into per layer through
//! [`crate::tiler::LayerData::line_simplify`].
//!
//! By default a line is pre-simplified for each zoom with Visvalingam-Whyatt
//! at an area of about eight tile pixels. That leaves dense HEC-RAS
//! breaklines with 1.2-1.6 times the vertices tippecanoe keeps at z12-z17,
//! where tippecanoe runs Douglas-Peucker at one tile pixel. An opted-in layer
//! instead runs Douglas-Peucker in Web Mercator at a tolerance given in tile
//! pixels (1/4096 of a tile side), at every zoom of the layer:
//!
//! - **One ranking per line.** Each vertex is ranked once with the deviation
//!   at which Douglas-Peucker would stop keeping it (the smaller of its own
//!   deviation and its parent's). A zoom keeps the vertices ranked above its
//!   tolerance, which is exactly Douglas-Peucker at that tolerance, so error
//!   does not accumulate over zooms and no per-zoom pass over the line is
//!   repeated.
//! - **Bounded error.** Every dropped vertex lies within the tolerance of the
//!   simplified line, measured in tile pixels at that zoom, before the usual
//!   snap to the tile pixel grid (at most half a pixel more).
//! - **Endpoints are kept**, so lines meeting at a node still meet, and every
//!   part keeps at least two vertices.
//! - **No feature is dropped.** A line that collapses after simplification
//!   falls back to its original vertices for that tile, as the default path
//!   does, and `keep_short_lines` still writes a stand-in where no tile would
//!   otherwise hold the feature.
//!
//! Simplifying lines independently moves their interior vertices, so this
//! must not be used on mesh faces, whose interior vertices are shared with the
//! cell polygons drawn over them.

use geo_types::{Coord, LineString, MultiLineString};

use crate::tiler::{Feature, Geometry};

/// Largest accepted tolerance, in tile pixels (1/4096 of a tile side).
pub const MAX_TOLERANCE_PX: f64 = 64.0;

/// Tile pixels per tile side, as the encoders use.
const EXTENT: f64 = 4096.0;

/// Web Mercator's latitude limit.
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

type Xy = [f64; 2];

/// One line part: its input vertices and the squared normalized Web Mercator
/// deviation above which each survives Douglas-Peucker.
struct Part {
    coords: Vec<Coord<f64>>,
    importance: Vec<f64>,
}

/// A line feature ranked for per-zoom simplification.
pub struct PreparedLine {
    parts: Vec<Part>,
    multi: bool,
}

/// True when `tolerance` is a usable per-layer tolerance in tile pixels.
pub fn valid_tolerance(tolerance: f64) -> bool {
    tolerance.is_finite() && tolerance > 0.0 && tolerance <= MAX_TOLERANCE_PX
}

/// Squared tolerance in normalized Web Mercator for `tolerance_px` at `zoom`.
pub fn tolerance2(tolerance_px: f64, zoom: u8) -> f64 {
    let t = tolerance_px / (EXTENT * (1u64 << zoom) as f64);
    t * t
}

fn to_mercator(c: &Coord<f64>) -> Xy {
    let lat = c.y.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    [
        (c.x + 180.0) / 360.0,
        0.5 - lat.tan().asinh() / (2.0 * std::f64::consts::PI),
    ]
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

/// Rank a line part. Both endpoints are always kept; every other vertex gets
/// the smaller of its deviation from the chord it splits and its parent's
/// rank, so keeping ranks above a tolerance reproduces Douglas-Peucker.
fn prepare_part(line: &LineString<f64>) -> Part {
    let coords = line.0.clone();
    let n = coords.len();
    let mut importance = vec![0.0; n];
    if n == 0 {
        return Part { coords, importance };
    }
    importance[0] = f64::INFINITY;
    importance[n - 1] = f64::INFINITY;
    if n < 3 {
        return Part { coords, importance };
    }
    let xy: Vec<Xy> = coords.iter().map(to_mercator).collect();
    let mut stack: Vec<(usize, usize, f64)> = vec![(0, n - 1, f64::INFINITY)];
    while let Some((start, end, parent)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let (a, b) = (&xy[start], &xy[end]);
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
        stack.push((start, best, rank));
        stack.push((best, end, rank));
    }
    Part { coords, importance }
}

/// Rank a feature's line parts; `None` for any other geometry type.
pub fn prepare_feature(feature: &Feature) -> Option<PreparedLine> {
    match &feature.geometry {
        Geometry::LineString(ls) => Some(PreparedLine {
            parts: vec![prepare_part(ls)],
            multi: false,
        }),
        Geometry::MultiLineString(mls) => Some(PreparedLine {
            parts: mls.0.iter().map(prepare_part).collect(),
            multi: true,
        }),
        _ => None,
    }
}

/// The line as Douglas-Peucker keeps it at squared tolerance `tolerance2`
/// (see [`tolerance2`]): input vertices only, endpoints always kept.
pub fn simplified(line: &PreparedLine, tolerance2: f64) -> Geometry {
    let parts: Vec<LineString<f64>> = line
        .parts
        .iter()
        .map(|part| {
            LineString(
                part.coords
                    .iter()
                    .zip(&part.importance)
                    .filter(|(_, rank)| **rank > tolerance2)
                    .map(|(c, _)| *c)
                    .collect(),
            )
        })
        .collect();
    if line.multi {
        Geometry::MultiLineString(MultiLineString(parts))
    } else {
        Geometry::LineString(parts.into_iter().next().unwrap_or(LineString(vec![])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiler::PropertyValue;

    fn feature(geometry: Geometry) -> Feature {
        Feature {
            id: None,
            geometry,
            properties: Vec::<PropertyValue>::new(),
        }
    }

    fn line(points: &[(f64, f64)]) -> LineString<f64> {
        LineString(points.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    fn vertices(g: &Geometry) -> usize {
        match g {
            Geometry::LineString(ls) => ls.0.len(),
            Geometry::MultiLineString(m) => m.0.iter().map(|l| l.0.len()).sum(),
            _ => 0,
        }
    }

    /// Reference Douglas-Peucker on Web Mercator points, recursive.
    fn reference(xy: &[Xy], t2: f64, keep: &mut Vec<bool>, start: usize, end: usize) {
        if end <= start + 1 {
            return;
        }
        let (mut best, mut best_d) = (start, -1.0);
        for i in start + 1..end {
            let d = segment_distance2(&xy[i], &xy[start], &xy[end]);
            if d > best_d {
                best_d = d;
                best = i;
            }
        }
        if best_d > t2 {
            keep[best] = true;
            reference(xy, t2, keep, start, best);
            reference(xy, t2, keep, best, end);
        }
    }

    #[test]
    fn ranks_reproduce_douglas_peucker_at_every_tolerance() {
        // A wiggly line near Houston.
        let pts: Vec<(f64, f64)> = (0..400)
            .map(|i| {
                let t = i as f64 / 400.0;
                (
                    -95.4 + 0.05 * t,
                    29.7 + 0.002 * (t * 37.0).sin() + 0.0005 * (t * 211.0).cos(),
                )
            })
            .collect();
        let ls = line(&pts);
        let prepared = prepare_feature(&feature(Geometry::LineString(ls.clone()))).unwrap();
        let xy: Vec<Xy> = ls.0.iter().map(to_mercator).collect();
        for zoom in 0..=18u8 {
            for px in [0.25, 0.5, 1.0, 4.0] {
                let t2 = tolerance2(px, zoom);
                let mut keep = vec![false; xy.len()];
                keep[0] = true;
                *keep.last_mut().unwrap() = true;
                reference(&xy, t2, &mut keep, 0, xy.len() - 1);
                let expected: Vec<Coord<f64>> =
                    ls.0.iter()
                        .zip(&keep)
                        .filter(|(_, k)| **k)
                        .map(|(c, _)| *c)
                        .collect();
                match simplified(&prepared, t2) {
                    Geometry::LineString(got) => assert_eq!(got.0, expected, "z{zoom} {px}px"),
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
    }

    #[test]
    fn endpoints_and_parts_survive() {
        let mls = MultiLineString(vec![
            line(&[(0.0, 0.0), (0.000001, 0.0000001), (0.000002, 0.0)]),
            line(&[(1.0, 1.0), (1.0, 1.0)]),
            line(&[(2.0, 2.0)]),
        ]);
        let prepared = prepare_feature(&feature(Geometry::MultiLineString(mls))).unwrap();
        match simplified(&prepared, tolerance2(64.0, 0)) {
            Geometry::MultiLineString(m) => {
                assert_eq!(m.0.len(), 3);
                assert_eq!(m.0[0].0.len(), 2);
                assert_eq!(m.0[1].0.len(), 2);
                assert_eq!(m.0[2].0.len(), 1);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn error_is_bounded_by_the_tolerance() {
        let pts: Vec<(f64, f64)> = (0..300)
            .map(|i| {
                let t = i as f64 / 300.0;
                (10.0 + 0.2 * t, 50.0 + 0.01 * (t * 53.0).sin())
            })
            .collect();
        let ls = line(&pts);
        let prepared = prepare_feature(&feature(Geometry::LineString(ls.clone()))).unwrap();
        for zoom in [4u8, 8, 12, 16] {
            let t2 = tolerance2(1.0, zoom);
            let Geometry::LineString(s) = simplified(&prepared, t2) else {
                unreachable!()
            };
            assert!(s.0.len() < ls.0.len() || zoom >= 12);
            let sxy: Vec<Xy> = s.0.iter().map(to_mercator).collect();
            for c in &ls.0 {
                let p = to_mercator(c);
                let d = sxy
                    .windows(2)
                    .map(|w| segment_distance2(&p, &w[0], &w[1]))
                    .fold(f64::INFINITY, f64::min);
                assert!(d <= t2 * (1.0 + 1e-9), "z{zoom}: {d} > {t2}");
            }
        }
    }

    #[test]
    fn other_geometries_are_not_prepared() {
        let p = Geometry::Point(geo_types::Point::new(0.0, 0.0));
        assert!(prepare_feature(&feature(p)).is_none());
        assert_eq!(
            vertices(&Geometry::LineString(line(&[(0.0, 0.0), (1.0, 1.0)]))),
            2
        );
    }

    #[test]
    fn tolerance_range() {
        assert!(valid_tolerance(0.5));
        assert!(valid_tolerance(64.0));
        assert!(!valid_tolerance(0.0));
        assert!(!valid_tolerance(64.5));
        assert!(!valid_tolerance(f64::NAN));
    }
}
