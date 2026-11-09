//! `LayerData::keep_short_lines`: every line feature at every zoom of its
//! layer, nothing else changed.

use freestiler_core::{
    engine::{generate_pmtiles, LayeredTiler, SilentReporter, TileConfig},
    geo_types::{Coord, LineString, MultiLineString, Polygon},
    mvt::Tile,
    pmtiles_writer::TileFormat,
    tiler::{tile_bounds, Feature, Geometry, LayerData, PropertyValue, TileCoord},
};
use prost::Message;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

const MIN_ZOOM: u8 = 0;
const MAX_ZOOM: u8 = 14;

fn config() -> TileConfig {
    TileConfig {
        tile_format: TileFormat::Mvt,
        min_zoom: MIN_ZOOM,
        max_zoom: MAX_ZOOM,
        base_zoom: None,
        simplification: true,
        drop_rate: None,
        cluster_distance: None,
        cluster_maxzoom: None,
        coalesce: false,
        gzip_level: None,
        buffer: Some(5.0),
        deduplicate: false,
        sort_values: false,
    }
}

fn feature(id: i64, geometry: Geometry) -> Feature {
    Feature {
        id: None,
        geometry,
        properties: vec![PropertyValue::Int(id)],
    }
}

fn line(points: &[(f64, f64)]) -> Geometry {
    Geometry::LineString(LineString::from(points.to_vec()))
}

/// Lines of every kind that collapse at some zoom, around Austin.
fn features() -> Vec<Feature> {
    let (x, y) = (-97.7, 30.3);
    let e = 1e-7; // about 0.01 m
    let mut out = vec![
        // Short lines: 0.01 m, 1 m, 30 m and 500 m.
        feature(0, line(&[(x, y), (x + e, y)])),
        feature(1, line(&[(x + 0.01, y), (x + 0.01 + 1e-5, y + 1e-5)])),
        feature(2, line(&[(x + 0.02, y), (x + 0.02, y + 3e-4)])),
        feature(3, line(&[(x + 0.03, y), (x + 0.035, y)])),
        // Zero length, and one with many vertices in a tiny loop.
        feature(4, line(&[(x + 0.04, y), (x + 0.04, y)])),
        feature(
            5,
            line(
                &(0..40)
                    .map(|i| {
                        let a = f64::from(i) * 0.15;
                        (x + 0.05 + 1e-6 * a.cos(), y + 1e-6 * a.sin())
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
        // Multi-part, every part short.
        feature(
            6,
            Geometry::MultiLineString(MultiLineString(vec![
                LineString::from(vec![(x + 0.06, y), (x + 0.06 + e, y)]),
                LineString::from(vec![(x + 0.06 + 2e-6, y), (x + 0.06 + 3e-6, y)]),
            ])),
        ),
        // Across the z1-z14 tile edge at 0 degrees of longitude.
        feature(7, line(&[(-e, 0.0), (e, 0.0)])),
        feature(8, line(&[(-1e-3, 0.0), (1e-3, 0.0)])),
        // On a tile corner.
        feature(9, line(&[(-e, e), (e, -e)])),
        // Long: always written.
        feature(10, line(&[(x, y), (x + 3.0, y + 2.0)])),
    ];
    // A row of short mesh-like faces along a street.
    for i in 0..50 {
        let a = x + 0.1 + f64::from(i) * 1e-4;
        out.push(feature(100 + i64::from(i), line(&[(a, y), (a + 2e-5, y)])));
    }
    out
}

fn layer(features: Vec<Feature>, keep: bool) -> LayerData {
    LayerData {
        name: "lines".into(),
        features,
        prop_names: vec!["id".into()],
        prop_types: vec!["integer".into()],
        min_zoom: MIN_ZOOM,
        max_zoom: MAX_ZOOM,
        polygon_simplify: None,
        keep_short_lines: keep,
        line_simplify: None,
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "freestiler_short_{}_{name}.pmtiles",
        std::process::id()
    ))
}

fn write(layers: &[LayerData], config: &TileConfig, name: &str) -> Vec<u8> {
    let path = temp(name);
    generate_pmtiles(layers, path.to_str().unwrap(), config, &SilentReporter).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// Decode an archive: tile -> id property -> decoded geometry commands, in
/// feature order within the tile.
type Decoded = BTreeMap<(u8, u32, u32), Vec<(i64, i32, Vec<u32>)>>;

fn decode(bytes: &[u8], name: &str) -> Decoded {
    let path = temp(&format!("{name}_decode"));
    std::fs::write(&path, bytes).unwrap();
    let mut archive = pmtiles2::PMTiles::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
    let ids: Vec<u64> = archive.tile_ids().into_iter().copied().collect();
    let mut out = Decoded::new();
    for id in ids {
        let (z, x, y) = pmtiles2::util::zxy(id).unwrap();
        let gz = archive.get_tile_by_id(id).unwrap().unwrap();
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        let tile = Tile::decode(raw.as_slice()).unwrap();
        let entry = out.entry((z, x as u32, y as u32)).or_default();
        for layer in &tile.layers {
            for f in &layer.features {
                let value = &layer.values[f.tags[1] as usize];
                let id = value
                    .sint_value
                    .or(value.int_value)
                    .or(value.uint_value.map(|v| v as i64))
                    .expect("integer id");
                entry.push((id, f.r#type.unwrap(), f.geometry.clone()));
            }
        }
    }
    drop(archive);
    let _ = std::fs::remove_file(&path);
    out
}

fn ids_at_zoom(decoded: &Decoded, zoom: u8) -> BTreeSet<i64> {
    decoded
        .iter()
        .filter(|((z, _, _), _)| *z == zoom)
        .flat_map(|(_, feats)| feats.iter().map(|f| f.0))
        .collect()
}

/// Decode an MVT line's commands to absolute tile positions.
fn line_points(commands: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let unzig = |v: u32| ((v >> 1) as i32) ^ -((v & 1) as i32);
    let (mut i, mut cx, mut cy) = (0, 0, 0);
    let mut parts = Vec::new();
    while i < commands.len() {
        let (id, count) = (commands[i] & 7, commands[i] >> 3);
        i += 1;
        assert!(id == 1 || id == 2, "unexpected command {id}");
        for _ in 0..count {
            let (dx, dy) = (unzig(commands[i]), unzig(commands[i + 1]));
            i += 2;
            cx += dx;
            cy += dy;
            if id == 1 {
                assert_eq!(count, 1, "MoveTo count must be 1 in a line");
                parts.push(vec![(cx, cy)]);
            } else {
                assert!(dx != 0 || dy != 0, "zero-length LineTo");
                parts.last_mut().unwrap().push((cx, cy));
            }
        }
    }
    parts
}

#[test]
fn every_line_is_present_at_every_zoom() {
    let off = decode(&write(&[layer(features(), false)], &config(), "a"), "a");
    let on = decode(&write(&[layer(features(), true)], &config(), "b"), "b");
    let all: BTreeSet<i64> = features()
        .iter()
        .map(|f| match f.properties[0] {
            PropertyValue::Int(i) => i,
            _ => unreachable!(),
        })
        .collect();
    let mut lost_before = 0;
    for z in MIN_ZOOM..=MAX_ZOOM {
        assert_eq!(ids_at_zoom(&on, z), all, "z{z}");
        lost_before += all.len() - ids_at_zoom(&off, z).len();
    }
    assert!(lost_before > 0, "the test must exercise the defect");
}

#[test]
fn lines_already_written_are_unchanged_and_only_absent_ones_are_added() {
    let off = decode(&write(&[layer(features(), false)], &config(), "c"), "c");
    let on = decode(&write(&[layer(features(), true)], &config(), "d"), "d");
    let mut added_by_zoom: BTreeMap<u8, usize> = BTreeMap::new();
    for (tile, feats) in &on {
        let before = off.get(tile).cloned().unwrap_or_default();
        let mut remaining = before.clone();
        for f in feats {
            if let Some(i) = remaining.iter().position(|g| g == f) {
                remaining.remove(i);
            } else {
                // Not in the earlier tile: it must be a stand-in for a feature
                // the earlier zoom had nowhere.
                *added_by_zoom.entry(tile.0).or_default() += 1;
                assert!(
                    !ids_at_zoom(&off, tile.0).contains(&f.0),
                    "id {} added at z{} though it was present",
                    f.0,
                    tile.0
                );
                let parts = line_points(&f.2);
                assert_eq!(parts.len(), 1);
                let [a, b] = parts[0][..] else {
                    panic!("stand-in has {} points", parts[0].len())
                };
                assert_eq!((a.0 - b.0).abs() + (a.1 - b.1).abs(), 1);
                for p in [a, b] {
                    assert!((0..=4096).contains(&p.0) && (0..=4096).contains(&p.1));
                }
            }
        }
        assert!(remaining.is_empty(), "{tile:?} lost {remaining:?}");
    }
    // The order of the features already there does not change either.
    for (tile, before) in &off {
        let kept: Vec<_> = on[tile].iter().filter(|f| before.contains(f)).collect();
        assert_eq!(kept, before.iter().collect::<Vec<_>>(), "{tile:?}");
    }
    assert!(!added_by_zoom.is_empty());
    // Added once per zoom: never more than the lines that were absent.
    for (&z, &added) in &added_by_zoom {
        assert_eq!(
            added,
            features().len() - ids_at_zoom(&off, z).len(),
            "z{z}: one stand-in per absent feature"
        );
    }
}

#[test]
fn stand_in_is_on_the_true_location() {
    // Line 0 is a 0.01 m line, under one pixel at every zoom here.
    let (lon, lat) = (-97.7, 30.3);
    let on = decode(&write(&[layer(features(), true)], &config(), "e"), "e");
    for ((z, x, y), feats) in &on {
        for f in feats.iter().filter(|f| f.0 == 0) {
            let b = tile_bounds(&TileCoord {
                z: *z,
                x: *x,
                y: *y,
            });
            let fx = (lon - b.min().x) / (b.max().x - b.min().x) * 4096.0;
            let south = b.min().y.to_radians().tan().asinh();
            let north = b.max().y.to_radians().tan().asinh();
            let fy = (north - f64::to_radians(lat).tan().asinh()) / (north - south) * 4096.0;
            let parts = line_points(&f.2);
            assert!(
                (f64::from(parts[0][0].0) - fx).abs() <= 0.51
                    && (f64::from(parts[0][0].1) - fy).abs() <= 0.51,
                "z{z}: {:?} vs {fx},{fy}",
                parts[0]
            );
        }
    }
}

#[test]
fn deterministic_and_independent_of_thread_count() {
    let run = |threads: usize, name: &str| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| write(&[layer(features(), true)], &config(), name))
    };
    let one = run(1, "t1");
    assert_eq!(one, run(1, "t1b"));
    assert_eq!(one, run(2, "t2"));
    assert_eq!(one, run(7, "t7"));
}

#[test]
fn layered_matches_in_memory() {
    let bytes = write(&[layer(features(), true)], &config(), "f");
    let path = temp("layered");
    let mut tiler = LayeredTiler::new(config()).unwrap();
    tiler
        .add_layer(layer(features(), true), &SilentReporter)
        .unwrap();
    tiler
        .finish(path.to_str().unwrap(), &SilentReporter)
        .unwrap();
    assert_eq!(bytes, std::fs::read(&path).unwrap());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn only_lines_change_and_mlt_is_untouched() {
    // A layer without lines is byte-identical either way.
    let square = |x: f64| {
        let e = 1e-7;
        feature(
            0,
            Geometry::Polygon(Polygon::new(
                LineString(vec![
                    Coord { x, y: 30.0 },
                    Coord { x: x + e, y: 30.0 },
                    Coord {
                        x: x + e,
                        y: 30.0 + e,
                    },
                    Coord { x, y: 30.0 },
                ]),
                vec![],
            )),
        )
    };
    let polygons = || vec![square(-97.7), square(-97.6)];
    for simplify in [None, Some(1.0)] {
        let mut a = layer(polygons(), false);
        let mut b = layer(polygons(), true);
        a.polygon_simplify = simplify;
        b.polygon_simplify = simplify;
        assert_eq!(
            write(&[a], &config(), "g"),
            write(&[b], &config(), "h"),
            "polygon_simplify {simplify:?}"
        );
    }

    // MLT writes the vertices of a collapsed line as they are: nothing to add.
    let mlt = TileConfig {
        tile_format: TileFormat::Mlt,
        ..config()
    };
    assert_eq!(
        write(&[layer(features(), false)], &mlt, "i"),
        write(&[layer(features(), true)], &mlt, "j")
    );
}

#[test]
fn dropped_features_stay_dropped() {
    // drop_rate removes features on purpose; the option does not undo that.
    let many: Vec<Feature> = (0..4000)
        .map(|i| {
            let a = -97.7 + f64::from(i) * 1e-4;
            feature(i64::from(i), line(&[(a, 30.3), (a + 5e-5, 30.3)]))
        })
        .collect();
    let dropping = TileConfig {
        drop_rate: Some(2.5),
        ..config()
    };
    let off = decode(&write(&[layer(many.clone(), false)], &dropping, "k"), "k");
    let on = decode(&write(&[layer(many.clone(), true)], &dropping, "l"), "l");
    let off_count = ids_at_zoom(&off, 8).len();
    let on_count = ids_at_zoom(&on, 8).len();
    assert!(
        on_count < many.len(),
        "z8 kept {on_count} of {}",
        many.len()
    );
    assert!(on_count >= off_count);
}

#[test]
fn a_mixed_layer_keeps_its_polygons_as_they_were() {
    let mut mixed = features();
    mixed.push(feature(
        999,
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (-97.0, 30.0),
                (-96.9, 30.0),
                (-96.9, 30.1),
                (-97.0, 30.0),
            ]),
            vec![],
        )),
    ));
    let off = decode(&write(&[layer(mixed.clone(), false)], &config(), "m"), "m");
    let on = decode(&write(&[layer(mixed, true)], &config(), "n"), "n");
    let polygon = |d: &Decoded| -> Vec<_> {
        d.iter()
            .flat_map(|(t, feats)| {
                feats
                    .iter()
                    .filter(|f| f.0 == 999)
                    .map(move |f| (*t, f.clone()))
            })
            .collect()
    };
    assert_eq!(polygon(&off), polygon(&on));
}

#[test]
fn every_option_combination_keeps_every_line() {
    let all = features().len();
    for (simplification, coalesce, buffer) in [
        (false, false, Some(5.0)),
        (true, true, Some(5.0)),
        (true, false, None),
        (true, false, Some(0.0)),
    ] {
        let options = TileConfig {
            simplification,
            coalesce,
            buffer,
            deduplicate: false,
            sort_values: false,
            ..config()
        };
        let on = decode(&write(&[layer(features(), true)], &options, "p"), "p");
        for z in MIN_ZOOM..=MAX_ZOOM {
            // Coalescing merges lines with equal properties; ids here are unique.
            assert_eq!(
                ids_at_zoom(&on, z).len(),
                all,
                "z{z} simplification={simplification} coalesce={coalesce} buffer={buffer:?}"
            );
        }
    }
}

#[test]
fn a_layer_zoom_range_is_respected() {
    let mut l = layer(features(), true);
    l.min_zoom = 3;
    l.max_zoom = 6;
    let on = decode(&write(&[l], &config(), "o"), "o");
    let zooms: BTreeSet<u8> = on.keys().map(|k| k.0).collect();
    assert_eq!(zooms, (3..=6).collect());
}
