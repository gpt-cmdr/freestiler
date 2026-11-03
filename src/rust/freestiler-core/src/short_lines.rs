//! Keep lines that collapse on the tile pixel grid, for a layer that opts in
//! through [`crate::tiler::LayerData::keep_short_lines`].
//!
//! A line whose vertices all snap to one tile pixel has no segment left. MVT
//! 2.1 section 4.3.3.2 says of every `LineTo` parameter pair "the `dX` and
//! `dY` MUST NOT both be `0`", and section 4.3.4.3 requires a `LineTo` with a
//! count above 0 after each `MoveTo`, so such a line cannot be written as
//! itself, and the encoder omits it.
//!
//! The replacement is a **one-tile-unit segment**, written once per zoom, only
//! for a feature that no tile of the zoom would otherwise contain:
//!
//! * It goes in the tile holding the line's first vertex (its *home tile*),
//!   never in the buffer of a neighbour, where a renderer would clip it away.
//! * It starts on the pixel that vertex snaps to and runs one unit along
//!   whichever axis the line itself mostly runs along, turning back if that
//!   would leave the tile. A line with no direction at all (identical
//!   vertices) runs along +x.
//!
//! Every vertex of a line that collapsed within its home tile is within half a
//! unit of the start on each axis, so no point of the true line is more than
//! about 0.7 units from the segment, and the far end of the segment is at most
//! about 1.6 units from the true line.
//!
//! Which features get one is decided per feature from the same clip and snap
//! the tile loop runs, so it does not depend on the thread count, and a line
//! that any tile already contains is left exactly as it was.

use geo_types::{Coord, LineString};

use crate::clip::buffered_tile_bounds;
use crate::simplify::line_collapses;
use crate::tiler::{lat_to_tile_y, lon_to_tile_x, tile_bounds, Geometry, TileCoord};

/// Tile coordinate extent (pixels per tile side)
const EXTENT: f64 = 4096.0;

/// The Web Mercator latitude limit: no tile covers anything beyond it.
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

/// The vertices of a line geometry; empty for points and polygons.
fn line_vertices(geom: &Geometry) -> Vec<Coord<f64>> {
    match geom {
        Geometry::LineString(ls) => ls.0.clone(),
        Geometry::MultiLineString(mls) => mls.0.iter().flat_map(|ls| ls.0.clone()).collect(),
        _ => Vec::new(),
    }
}

fn on_the_map(c: &Coord<f64>) -> bool {
    c.x.is_finite()
        && c.y.is_finite()
        && (-180.0..=180.0).contains(&c.x)
        && (-MAX_LATITUDE..=MAX_LATITUDE).contains(&c.y)
}

/// True when a single line is certainly written at this zoom, found without
/// clipping or allocating. A line inside the clip rectangle of the tile holding
/// its first vertex passes the clip with all its vertices, in that tile, so its
/// pixels there are the ones this looks at. `false` says nothing: most lines
/// are settled here, and the rest take the tile loop's own clip and snap.
pub fn written_in_home_tile(geom: &Geometry, zoom: u8, buffer_fraction: f64) -> bool {
    let Geometry::LineString(ls) = geom else {
        return false;
    };
    let Some(first) = ls.0.first().filter(|c| on_the_map(c)) else {
        return false;
    };
    let coord = TileCoord {
        z: zoom,
        x: lon_to_tile_x(first.x, zoom),
        y: lat_to_tile_y(first.y, zoom),
    };
    let rect = buffered_tile_bounds(&coord, buffer_fraction);
    ls.0.iter().all(|c| {
        c.x >= rect.min().x && c.x <= rect.max().x && c.y >= rect.min().y && c.y <= rect.max().y
    }) && !line_collapses(geom, &coord)
}

/// The minimal form of a line feature at a zoom: its home tile and a
/// one-tile-unit segment there. `None` for anything but a line, and for a line
/// with no vertex on the Web Mercator map, which no tile covers.
pub fn minimal_line(geom: &Geometry, zoom: u8) -> Option<(TileCoord, Geometry)> {
    let vertices = line_vertices(geom);
    let anchor = vertices.iter().find(|c| on_the_map(c))?;
    let coord = TileCoord {
        z: zoom,
        x: lon_to_tile_x(anchor.x, zoom),
        y: lat_to_tile_y(anchor.y, zoom),
    };
    let bounds = tile_bounds(&coord);
    let (west, east) = (bounds.min().x, bounds.max().x);
    let south_merc = bounds.min().y.to_radians().tan().asinh();
    let north_merc = bounds.max().y.to_radians().tan().asinh();
    let merc_range = north_merc - south_merc;
    let to_tile = |c: &Coord<f64>| {
        (
            (c.x - west) / (east - west) * EXTENT,
            (north_merc - c.y.to_radians().tan().asinh()) / merc_range * EXTENT,
        )
    };
    let to_lonlat = |x: f64, y: f64| Coord {
        x: west + x / EXTENT * (east - west),
        y: (north_merc - y / EXTENT * merc_range)
            .sinh()
            .atan()
            .to_degrees(),
    };

    let (ax, ay) = to_tile(anchor);
    let (px, py) = (ax.round().clamp(0.0, EXTENT), ay.round().clamp(0.0, EXTENT));

    // The direction of the vertex farthest from the first; the earliest of
    // equally far vertices, so the choice depends only on the vertex order.
    let (mut dx, mut dy, mut farthest) = (0.0f64, 0.0f64, 0.0f64);
    for c in vertices.iter().filter(|c| on_the_map(c)) {
        let (x, y) = to_tile(c);
        let d = (x - ax).powi(2) + (y - ay).powi(2);
        if d > farthest {
            (dx, dy, farthest) = (x - ax, y - ay, d);
        }
    }
    let (mut step_x, mut step_y) = if dy.abs() > dx.abs() {
        (0.0, dy.signum())
    } else if dx != 0.0 {
        (dx.signum(), 0.0)
    } else {
        (1.0, 0.0)
    };
    if !(0.0..=EXTENT).contains(&(px + step_x)) {
        step_x = -step_x;
    }
    if !(0.0..=EXTENT).contains(&(py + step_y)) {
        step_y = -step_y;
    }

    Some((
        coord,
        Geometry::LineString(LineString(vec![
            to_lonlat(px, py),
            to_lonlat(px + step_x, py + step_y),
        ])),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quantize::{lat_to_tile_coord, lon_to_tile_coord};
    use geo_types::MultiLineString;

    fn encoded(geom: &Geometry, coord: &TileCoord) -> Vec<(i32, i32)> {
        let Geometry::LineString(ls) = geom else {
            panic!("not a line")
        };
        let b = tile_bounds(coord);
        ls.0.iter()
            .map(|c| {
                (
                    lon_to_tile_coord(c.x, b.min().x, b.max().x),
                    lat_to_tile_coord(c.y, b.min().y, b.max().y),
                )
            })
            .collect()
    }

    fn line(points: &[(f64, f64)]) -> Geometry {
        Geometry::LineString(LineString::from(points.to_vec()))
    }

    /// The lon/lat of a position in tile coordinates of a tile.
    fn at(coord: &TileCoord, x: f64, y: f64) -> (f64, f64) {
        let b = tile_bounds(coord);
        let (w, e) = (b.min().x, b.max().x);
        let s = b.min().y.to_radians().tan().asinh();
        let n = b.max().y.to_radians().tan().asinh();
        (
            w + x / EXTENT * (e - w),
            (n - y / EXTENT * (n - s)).sinh().atan().to_degrees(),
        )
    }

    const TILE: TileCoord = TileCoord {
        z: 10,
        x: 300,
        y: 400,
    };

    #[test]
    fn is_one_unit_on_the_snapped_pixel_along_the_line() {
        for (from, to, want) in [
            ((100.1, 200.1), (100.4, 200.2), ((100, 200), (101, 200))),
            ((100.4, 200.1), (100.1, 200.2), ((100, 200), (99, 200))),
            ((100.1, 200.1), (100.2, 200.4), ((100, 200), (100, 201))),
            ((100.1, 200.4), (100.2, 200.1), ((100, 200), (100, 199))),
        ] {
            let a = at(&TILE, from.0, from.1);
            let b = at(&TILE, to.0, to.1);
            let (coord, geom) = minimal_line(&line(&[a, b]), TILE.z).unwrap();
            assert_eq!(coord, TILE);
            assert_eq!(encoded(&geom, &coord), vec![want.0, want.1]);
        }
    }

    #[test]
    fn a_line_with_no_direction_runs_along_x() {
        let a = at(&TILE, 7.2, 9.2);
        let (coord, geom) = minimal_line(&line(&[a, a]), TILE.z).unwrap();
        assert_eq!(encoded(&geom, &coord), vec![(7, 9), (8, 9)]);
    }

    #[test]
    fn stays_inside_the_tile_at_its_edges() {
        // Running off each side turns back into the tile. The first vertex
        // lies inside the tile, so the home tile is this one.
        for (from, to, want) in [
            ((4095.8, 50.0), (4095.9, 50.0), ((4096, 50), (4095, 50))),
            ((0.2, 50.0), (0.1, 50.0), ((0, 50), (1, 50))),
            ((50.0, 4095.8), (50.0, 4095.9), ((50, 4096), (50, 4095))),
            ((50.0, 0.2), (50.0, 0.1), ((50, 0), (50, 1))),
        ] {
            let a = at(&TILE, from.0, from.1);
            let b = at(&TILE, to.0, to.1);
            let (coord, geom) = minimal_line(&line(&[a, b]), TILE.z).unwrap();
            assert_eq!(coord, TILE, "{from:?}");
            assert_eq!(encoded(&geom, &coord), vec![want.0, want.1]);
        }
    }

    #[test]
    fn multipart_uses_the_first_vertex_and_only_lines_qualify() {
        let a = at(&TILE, 10.1, 10.1);
        let b = at(&TILE, 10.3, 10.1);
        let multi = Geometry::MultiLineString(MultiLineString(vec![
            LineString::<f64>::new(vec![]),
            LineString::from(vec![a, b]),
        ]));
        let (coord, geom) = minimal_line(&multi, TILE.z).unwrap();
        assert_eq!(encoded(&geom, &coord), vec![(10, 10), (11, 10)]);
        assert!(minimal_line(&Geometry::Point(geo_types::Point::new(a.0, a.1)), 10).is_none());
    }

    #[test]
    fn only_a_line_certainly_written_is_settled_in_its_home_tile() {
        let buffer = 5.0 / 256.0;
        let long = line(&[at(&TILE, 10.0, 10.0), at(&TILE, 20.0, 10.0)]);
        assert!(written_in_home_tile(&long, TILE.z, buffer));
        // Within one pixel: not written.
        let short = line(&[at(&TILE, 10.1, 10.1), at(&TILE, 10.3, 10.1)]);
        assert!(!written_in_home_tile(&short, TILE.z, buffer));
        // Inside the buffer is inside the clip; beyond it needs the real clip.
        let in_buffer = line(&[at(&TILE, 4090.0, 10.0), at(&TILE, 4100.0, 10.0)]);
        assert!(written_in_home_tile(&in_buffer, TILE.z, buffer));
        let beyond = line(&[at(&TILE, 4090.0, 10.0), at(&TILE, 4300.0, 10.0)]);
        assert!(!written_in_home_tile(&beyond, TILE.z, buffer));
        // Multi-part lines and lines off the map always take the real path.
        let multi = Geometry::MultiLineString(MultiLineString(vec![LineString::from(vec![
            at(&TILE, 10.0, 10.0),
            at(&TILE, 20.0, 10.0),
        ])]));
        assert!(!written_in_home_tile(&multi, TILE.z, buffer));
        assert!(!written_in_home_tile(
            &line(&[(10.0, 89.0), (10.0, 80.0)]),
            3,
            buffer
        ));
    }

    #[test]
    fn no_tile_covers_a_line_off_the_map() {
        assert!(minimal_line(&line(&[(10.0, 89.0), (10.0, 89.5)]), 3).is_none());
        // A vertex on the map is enough.
        assert!(minimal_line(&line(&[(10.0, 89.0), (10.0, 80.0)]), 3).is_some());
        assert!(minimal_line(&line(&[]), 3).is_none());
    }
}
