//! Per-zoom polygon simplification (`LayerData::polygon_simplify`): no feature
//! is lost at any zoom, the tiles stay the same, the error stays small, and
//! output does not depend on the pipeline or the thread count.

use freestiler_core::{
    engine::{generate_pmtiles, LayeredTiler, SilentReporter, TileConfig},
    geo_types::{Coord, LineString, MultiPolygon, Point, Polygon},
    mvt::Tile,
    pmtiles_writer::TileFormat,
    tiler::{Feature, Geometry, LayerData},
};
use prost::Message;
use std::collections::{BTreeMap, BTreeSet};
use std::f64::consts::TAU;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

const MAX_ZOOM: u8 = 14;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "freestiler_polygon_zoom_{}_{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A wobbling star ring around a centre, dense and concave.
fn star(cx: f64, cy: f64, radius: f64, vertices: usize) -> LineString<f64> {
    let mut coords: Vec<Coord<f64>> = (0..vertices)
        .map(|i| {
            let a = i as f64 / vertices as f64 * TAU;
            let r = radius * (1.0 + 0.3 * (9.0 * a).sin() + 0.002 * (211.0 * a).sin());
            Coord {
                x: cx + r * a.cos(),
                y: cy + r * a.sin(),
            }
        })
        .collect();
    coords.push(coords[0]);
    LineString(coords)
}

fn feature(id: u64, geometry: Geometry) -> Feature {
    Feature {
        id: Some(id),
        geometry,
        properties: vec![],
    }
}

/// One dense polygon with a hole, a multipolygon, tiny polygons (one exactly on
/// a tile boundary), and a line and a point that keep the regular path.
fn features() -> Vec<Feature> {
    let (cx, cy) = (-98.7, 29.95);
    let mut out = vec![feature(
        1,
        Geometry::Polygon(Polygon::new(
            star(cx, cy, 0.15, 6000),
            vec![star(cx, cy, 0.05, 900)],
        )),
    )];
    out.push(feature(
        2,
        Geometry::MultiPolygon(MultiPolygon(vec![
            Polygon::new(star(-98.2, 29.7, 0.02, 300), vec![]),
            Polygon::new(star(-98.1, 29.7, 0.02, 300), vec![]),
        ])),
    ));
    for i in 0..40u64 {
        let (x, y) = (-98.9 + i as f64 * 0.02, 29.5 + (i % 7) as f64 * 0.01);
        out.push(feature(
            10 + i,
            Geometry::Polygon(Polygon::new(star(x, y, 2e-6, 12), vec![])),
        ));
    }
    // Centred on the z2 meridian boundary at -90 degrees.
    out.push(feature(
        60,
        Geometry::Polygon(Polygon::new(star(-90.0, 29.5, 1e-5, 12), vec![])),
    ));
    out.push(feature(
        61,
        Geometry::LineString(LineString::from(vec![(-98.8, 29.9), (-98.6, 30.0)])),
    ));
    out.push(feature(62, Geometry::Point(Point::new(-98.7, 29.95))));
    out
}

fn layer(tolerance: Option<f64>) -> LayerData {
    LayerData {
        name: "land_cover".into(),
        features: features(),
        prop_names: vec![],
        prop_types: vec![],
        min_zoom: 0,
        max_zoom: MAX_ZOOM,
        polygon_simplify: tolerance,
    }
}

fn config(format: TileFormat) -> TileConfig {
    TileConfig {
        tile_format: format,
        min_zoom: 0,
        max_zoom: MAX_ZOOM,
        base_zoom: None,
        simplification: true,
        drop_rate: None,
        cluster_distance: None,
        cluster_maxzoom: None,
        coalesce: false,
        gzip_level: None,
        buffer: Some(5.0),
    }
}

fn rings(commands: &[u32]) -> Vec<Vec<(i64, i64)>> {
    let unzig = |v: u32| ((v >> 1) as i64) ^ -((v & 1) as i64);
    let (mut out, mut current, mut i, mut x, mut y) = (Vec::new(), Vec::new(), 0, 0, 0);
    while i < commands.len() {
        let (id, count) = (commands[i] & 7, commands[i] >> 3);
        i += 1;
        if id == 7 {
            out.push(std::mem::take(&mut current));
            continue;
        }
        for _ in 0..count {
            x += unzig(commands[i]);
            y += unzig(commands[i + 1]);
            i += 2;
            current.push((x, y));
        }
    }
    out
}

fn signed_area(ring: &[(i64, i64)]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            (a.0 * b.1 - b.0 * a.1) as f64
        })
        .sum::<f64>()
        / 2.0
}

#[derive(Default)]
struct Zoom {
    tiles: BTreeSet<u64>,
    ids: BTreeSet<u64>,
    vertices: usize,
    /// Polygon area (exteriors less holes) in tile pixels.
    area: f64,
    points: BTreeSet<(u64, i64, i64)>,
}

/// Decode an MVT archive per zoom.
fn read(path: &str) -> BTreeMap<u8, Zoom> {
    let mut archive = pmtiles2::PMTiles::from_reader(fs::File::open(path).unwrap()).unwrap();
    let ids: Vec<u64> = archive.tile_ids().into_iter().copied().collect();
    let mut zooms: BTreeMap<u8, Zoom> = BTreeMap::new();
    for id in ids {
        let z = (0..32u8)
            .find(|&z| pmtiles2::util::tile_id(z + 1, 0, 0) > id)
            .unwrap();
        let compressed = archive.get_tile_by_id(id).unwrap().unwrap();
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        let zoom = zooms.entry(z).or_default();
        zoom.tiles.insert(id);
        for layer in Tile::decode(raw.as_slice()).unwrap().layers {
            for feature in layer.features {
                zoom.ids.insert(feature.id.unwrap());
                if feature.r#type != Some(3) {
                    continue;
                }
                for ring in rings(&feature.geometry) {
                    zoom.vertices += ring.len();
                    zoom.area += signed_area(&ring);
                    for (x, y) in &ring {
                        zoom.points.insert((id, *x, *y));
                    }
                }
            }
        }
    }
    zooms
}

fn write(path: &str, tolerance: Option<f64>) {
    generate_pmtiles(
        &[layer(tolerance)],
        path,
        &config(TileFormat::Mvt),
        &SilentReporter,
    )
    .unwrap();
}

#[test]
fn every_feature_is_at_every_zoom() {
    let scratch = Scratch::new();
    let path = scratch.file("on.pmtiles");
    write(&path, Some(1.0));
    let all: BTreeSet<u64> = features().iter().map(|f| f.id.unwrap()).collect();
    let zooms = read(&path);
    for z in 0..=MAX_ZOOM {
        let ids = &zooms[&z].ids;
        let missing: Vec<_> = all.difference(ids).collect();
        assert!(missing.is_empty(), "z{z} lost {missing:?}");
    }
}

#[test]
fn large_tolerance_still_keeps_every_feature() {
    let scratch = Scratch::new();
    let path = scratch.file("on.pmtiles");
    write(&path, Some(64.0));
    let all: BTreeSet<u64> = features().iter().map(|f| f.id.unwrap()).collect();
    let zooms = read(&path);
    for z in 0..=MAX_ZOOM {
        assert_eq!(zooms[&z].ids, all, "z{z}");
    }
}

#[test]
fn tiles_area_and_max_zoom_agree_with_the_regular_path() {
    let scratch = Scratch::new();
    let (on, off) = (scratch.file("on.pmtiles"), scratch.file("off.pmtiles"));
    write(&on, Some(1.0));
    write(&off, None);
    let (on, off) = (read(&on), read(&off));
    for z in 8..=MAX_ZOOM {
        assert_eq!(on[&z].tiles, off[&z].tiles, "z{z} tiles differ");
        let change = (on[&z].area - off[&z].area).abs() / off[&z].area;
        assert!(change < 0.002, "z{z}: area changed by {change}");
    }
    // Low zooms carry far fewer vertices.
    assert!(on[&8].vertices * 2 < off[&8].vertices);
    // The maximum zoom keeps the input: the same pixels, bar a rare rounding.
    let (a, b) = (&on[&MAX_ZOOM].points, &off[&MAX_ZOOM].points);
    let differ = a.symmetric_difference(b).count();
    assert!(
        differ * 100 <= a.len(),
        "{differ} of {} vertices differ at the maximum zoom",
        a.len()
    );
}

#[test]
fn a_tile_inside_a_hole_stays_empty() {
    let scratch = Scratch::new();
    let path = scratch.file("hole.pmtiles");
    let layer = LayerData {
        name: "holed".into(),
        features: vec![feature(
            1,
            Geometry::Polygon(Polygon::new(
                star(-98.7, 29.95, 0.2, 400),
                vec![star(-98.7, 29.95, 0.1, 400)],
            )),
        )],
        prop_names: vec![],
        prop_types: vec![],
        min_zoom: 13,
        max_zoom: 13,
        polygon_simplify: Some(1.0),
    };
    let mut config = config(TileFormat::Mvt);
    config.min_zoom = 13;
    config.max_zoom = 13;
    generate_pmtiles(&[layer], &path, &config, &SilentReporter).unwrap();
    // The tile holding the centre lies wholly inside the hole.
    let x = ((-98.7 + 180.0) / 360.0 * 8192.0_f64).floor() as u64;
    let lat = 29.95_f64.to_radians();
    let y = ((1.0 - lat.tan().asinh() / std::f64::consts::PI) / 2.0 * 8192.0).floor() as u64;
    let centre = pmtiles2::util::tile_id(13, x, y);
    let zooms = read(&path);
    assert!(!zooms[&13].tiles.is_empty());
    assert!(!zooms[&13].tiles.contains(&centre));
}

#[test]
fn layered_is_byte_identical_and_thread_count_does_not_matter() {
    let scratch = Scratch::new();
    for format in [TileFormat::Mvt, TileFormat::Mlt] {
        let one = scratch.file(&format!("one_{format:?}.pmtiles"));
        let four = scratch.file(&format!("four_{format:?}.pmtiles"));
        let layered = scratch.file(&format!("layered_{format:?}.pmtiles"));
        for (threads, path) in [(1, &one), (4, &four)] {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| {
                    generate_pmtiles(&[layer(Some(1.0))], path, &config(format), &SilentReporter)
                        .unwrap()
                });
        }
        let mut tiler = LayeredTiler::new(config(format)).unwrap();
        tiler.add_layer(layer(Some(1.0)), &SilentReporter).unwrap();
        tiler.finish(&layered, &SilentReporter).unwrap();
        let bytes = fs::read(&one).unwrap();
        assert!(bytes.len() > 1000);
        assert!(bytes == fs::read(&four).unwrap(), "{format:?}: threads");
        assert!(bytes == fs::read(&layered).unwrap(), "{format:?}: layered");
    }
}

#[test]
fn layers_without_polygons_are_unchanged_by_the_option() {
    let scratch = Scratch::new();
    let make = |tolerance| LayerData {
        name: "lines".into(),
        features: vec![
            feature(
                1,
                Geometry::LineString(LineString::from(vec![(-98.8, 29.9), (-98.6, 30.0)])),
            ),
            feature(2, Geometry::Point(Point::new(-98.7, 29.95))),
        ],
        prop_names: vec![],
        prop_types: vec![],
        min_zoom: 0,
        max_zoom: 12,
        polygon_simplify: tolerance,
    };
    let (a, b) = (scratch.file("off.pmtiles"), scratch.file("on.pmtiles"));
    let mut config = config(TileFormat::Mvt);
    config.max_zoom = 12;
    generate_pmtiles(&[make(None)], &a, &config, &SilentReporter).unwrap();
    generate_pmtiles(&[make(Some(1.0))], &b, &config, &SilentReporter).unwrap();
    assert!(fs::read(a).unwrap() == fs::read(b).unwrap());
}

#[test]
fn only_the_opted_in_layer_changes() {
    // A second layer in the same tileset must keep its regular-path bytes: its
    // tiles do not depend on the first layer's option.
    let scratch = Scratch::new();
    let mesh = || LayerData {
        name: "mesh_cells".into(),
        features: (0..40u64)
            .map(|i| {
                let x = -98.7 + i as f64 * 0.001;
                feature(
                    i + 1,
                    Geometry::Polygon(Polygon::new(
                        LineString::from(vec![
                            (x, 29.9),
                            (x + 0.001, 29.9),
                            (x + 0.001, 29.901),
                            (x, 29.901),
                            (x, 29.9),
                        ]),
                        vec![],
                    )),
                )
            })
            .collect(),
        prop_names: vec![],
        prop_types: vec![],
        min_zoom: 10,
        max_zoom: MAX_ZOOM,
        polygon_simplify: None,
    };
    let (a, b) = (scratch.file("alone.pmtiles"), scratch.file("with.pmtiles"));
    let cfg = config(TileFormat::Mvt);
    generate_pmtiles(&[mesh()], &a, &cfg, &SilentReporter).unwrap();
    generate_pmtiles(&[layer(Some(1.0)), mesh()], &b, &cfg, &SilentReporter).unwrap();
    // Compare the mesh layer's encoded messages, tile by tile.
    let mesh_layers = |path: &str| {
        let mut archive = pmtiles2::PMTiles::from_reader(fs::File::open(path).unwrap()).unwrap();
        let ids: Vec<u64> = archive.tile_ids().into_iter().copied().collect();
        let mut out = BTreeMap::new();
        for id in ids {
            let compressed = archive.get_tile_by_id(id).unwrap().unwrap();
            let mut raw = Vec::new();
            flate2::read::GzDecoder::new(compressed.as_slice())
                .read_to_end(&mut raw)
                .unwrap();
            for layer in Tile::decode(raw.as_slice()).unwrap().layers {
                if layer.name == "mesh_cells" {
                    out.insert(id, layer.encode_to_vec());
                }
            }
        }
        out
    };
    let (alone, with) = (mesh_layers(&a), mesh_layers(&b));
    assert!(!alone.is_empty());
    assert!(alone == with);
}

#[test]
fn a_bad_tolerance_is_refused() {
    let scratch = Scratch::new();
    let path = scratch.file("bad.pmtiles");
    for tolerance in [0.0, -1.0, 65.0, f64::NAN, f64::INFINITY] {
        let error = generate_pmtiles(
            &[layer(Some(tolerance))],
            &path,
            &config(TileFormat::Mvt),
            &SilentReporter,
        )
        .unwrap_err();
        assert!(error.contains("polygon_simplify"), "{error}");
        let mut tiler = LayeredTiler::new(config(TileFormat::Mvt)).unwrap();
        assert!(tiler
            .add_layer(layer(Some(tolerance)), &SilentReporter)
            .is_err());
    }
}
