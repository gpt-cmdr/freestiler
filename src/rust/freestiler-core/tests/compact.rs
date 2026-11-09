//! Opt-in archive and tile compaction for sparse tilesets:
//! `TileConfig::deduplicate` (identical tiles stored once, run-length
//! directory), `TileConfig::sort_values` (keys and sorted values ahead of
//! features), and `LayerData::line_simplify` (per-zoom Douglas-Peucker on
//! lines). None of them may lose a feature, and the first two may not change
//! any decoded tile.

use freestiler_core::{
    engine::{generate_pmtiles, LayeredTiler, SilentReporter, TileConfig},
    geo_types::{LineString, Polygon},
    mvt::{Tile, Value},
    pmtiles_writer::TileFormat,
    tiler::{Feature, Geometry, LayerData, PropertyValue},
};
use prost::Message;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

const MIN_ZOOM: u8 = 6;
const MAX_ZOOM: u8 = 14;

fn layer(name: &str, features: Vec<Feature>, props: &[&str]) -> LayerData {
    LayerData {
        name: name.into(),
        features,
        prop_names: props.iter().map(|p| p.to_string()).collect(),
        prop_types: props.iter().map(|_| "string".to_string()).collect(),
        min_zoom: MIN_ZOOM,
        max_zoom: MAX_ZOOM,
        polygon_simplify: None,
        keep_short_lines: false,
        line_simplify: None,
    }
}

/// A model extent: one polygon covering many whole tiles at high zoom, so
/// most of its tiles are identical.
fn extent() -> LayerData {
    let ring = LineString::from(vec![
        (-77.80, 40.90),
        (-77.60, 40.90),
        (-77.60, 41.05),
        (-77.80, 41.05),
        (-77.80, 40.90),
    ]);
    layer(
        "extent",
        vec![Feature {
            id: Some(1),
            geometry: Geometry::Polygon(Polygon::new(ring, vec![])),
            properties: vec![PropertyValue::String("model".into())],
        }],
        &["name"],
    )
}

/// Wiggly breaklines with many property values, plus very short ones.
fn lines() -> LayerData {
    let mut features = Vec::new();
    for k in 0..40u64 {
        let y0 = 40.92 + k as f64 * 0.003;
        let points: Vec<(f64, f64)> = (0..200)
            .map(|i| {
                let t = i as f64 / 200.0;
                (
                    -77.78 + 0.15 * t,
                    y0 + 0.0008 * (t * 41.0 + k as f64).sin() + 0.0002 * (t * 233.0).cos(),
                )
            })
            .collect();
        features.push(Feature {
            id: Some(100 + k),
            geometry: Geometry::LineString(LineString::from(points)),
            properties: vec![
                PropertyValue::String(format!("line {}", 39 - k)),
                PropertyValue::Double(100.0 + (k % 7) as f64 * 0.5),
                PropertyValue::Int((k % 3) as i64),
            ],
        });
    }
    for k in 0..10u64 {
        let (x, y) = (-77.7 + k as f64 * 0.01, 41.0);
        features.push(Feature {
            id: Some(200 + k),
            geometry: Geometry::LineString(LineString::from(vec![
                (x, y),
                (x + 0.00002, y + 0.00001),
                (x + 0.00004, y),
            ])),
            properties: vec![
                PropertyValue::String(format!("short {k}")),
                PropertyValue::Double(1.25),
                PropertyValue::Null,
            ],
        });
    }
    layer("breaklines", features, &["Name", "spacing", "repeats"])
}

fn config(deduplicate: bool, sort_values: bool) -> TileConfig {
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
        gzip_level: Some(6),
        buffer: Some(5.0),
        deduplicate,
        sort_values,
    }
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "freestiler_compact_{}_{}.pmtiles",
        name,
        std::process::id()
    ))
}

fn build(name: &str, layers: Vec<LayerData>, config: &TileConfig) -> PathBuf {
    let out = temp(name);
    generate_pmtiles(&layers, out.to_str().unwrap(), config, &SilentReporter).unwrap();
    out
}

/// Every addressed tile, decompressed, by tile ID.
fn tiles(path: &Path) -> BTreeMap<u64, Vec<u8>> {
    let mut archive = pmtiles2::PMTiles::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    let ids: Vec<u64> = archive.tile_ids().into_iter().copied().collect();
    ids.into_iter()
        .map(|id| {
            let gz = archive.get_tile_by_id(id).unwrap().unwrap();
            let mut raw = Vec::new();
            flate2::read::GzDecoder::new(gz.as_slice())
                .read_to_end(&mut raw)
                .unwrap();
            (id, raw)
        })
        .collect()
}

fn header(path: &Path) -> pmtiles2::Header {
    pmtiles2::Header::from_reader(&mut std::fs::File::open(path).unwrap()).unwrap()
}

/// A tile as decoders see it: per layer, each feature's id, type, geometry
/// and (key, value) properties, in feature order.
type Decoded = Vec<(
    String,
    Vec<(Option<u64>, Option<i32>, Vec<u32>, Vec<(String, Vec<u8>)>)>,
)>;

fn decoded(raw: &[u8]) -> Decoded {
    Tile::decode(raw)
        .unwrap()
        .layers
        .into_iter()
        .map(|l| {
            let features = l
                .features
                .iter()
                .map(|f| {
                    let props = f
                        .tags
                        .chunks(2)
                        .map(|kv| {
                            (
                                l.keys[kv[0] as usize].clone(),
                                l.values[kv[1] as usize].encode_to_vec(),
                            )
                        })
                        .collect();
                    (f.id, f.r#type, f.geometry.clone(), props)
                })
                .collect();
            (l.name, features)
        })
        .collect()
}

fn zoom_of(id: u64) -> u8 {
    (0..32u8)
        .find(|&z| pmtiles2::util::tile_id(z + 1, 0, 0) > id)
        .unwrap()
}

/// Feature IDs per (layer, zoom).
fn ids(path: &Path) -> BTreeMap<(String, u8), BTreeSet<u64>> {
    let mut out: BTreeMap<(String, u8), BTreeSet<u64>> = BTreeMap::new();
    for (id, raw) in tiles(path) {
        for l in Tile::decode(raw.as_slice()).unwrap().layers {
            out.entry((l.name.clone(), zoom_of(id)))
                .or_default()
                .extend(l.features.iter().filter_map(|f| f.id));
        }
    }
    out
}

fn vertices(path: &Path, name: &str) -> usize {
    tiles(path)
        .values()
        .flat_map(|raw| Tile::decode(raw.as_slice()).unwrap().layers)
        .filter(|l| l.name == name)
        .flat_map(|l| l.features)
        .map(|f| f.geometry.len())
        .sum()
}

#[test]
fn deduplicate_stores_identical_tiles_once() {
    let plain = build("plain", vec![extent(), lines()], &config(false, false));
    let dedup = build("dedup", vec![extent(), lines()], &config(true, false));

    let (a, b) = (tiles(&plain), tiles(&dedup));
    assert_eq!(a, b, "every tile must decompress to the same bytes");

    let (ha, hb) = (header(&plain), header(&dedup));
    assert_eq!(ha.num_addressed_tiles, a.len() as u64);
    assert_eq!(ha.num_tile_content, ha.num_addressed_tiles);
    assert_eq!(hb.num_addressed_tiles, ha.num_addressed_tiles);
    let distinct: BTreeSet<&Vec<u8>> = b.values().collect();
    assert_eq!(hb.num_tile_content, distinct.len() as u64);
    assert!(
        hb.num_tile_content < hb.num_addressed_tiles,
        "the fixture has repeated tiles"
    );
    assert!(
        hb.num_tile_entries < hb.num_addressed_tiles,
        "repeats should form runs"
    );
    assert!(hb.clustered);
    assert!(hb.tile_data_length < ha.tile_data_length);
    assert!(std::fs::metadata(&dedup).unwrap().len() < std::fs::metadata(&plain).unwrap().len());

    // The same on the layered path.
    let layered = temp("dedup_layered");
    let mut tiler = LayeredTiler::new(config(true, false)).unwrap();
    tiler.add_layer(extent(), &SilentReporter).unwrap();
    tiler.add_layer(lines(), &SilentReporter).unwrap();
    tiler
        .finish(layered.to_str().unwrap(), &SilentReporter)
        .unwrap();
    assert_eq!(
        std::fs::read(&layered).unwrap(),
        std::fs::read(&dedup).unwrap()
    );

    for p in [plain, dedup, layered] {
        let _ = std::fs::remove_file(p);
    }
}

#[test]
fn deduplicate_off_keeps_the_previous_archive() {
    // Off means one entry and one copy per tile, as before the option.
    let plain = build("off", vec![extent(), lines()], &config(false, false));
    let h = header(&plain);
    assert_eq!(h.num_tile_entries, h.num_addressed_tiles);
    assert_eq!(h.num_tile_content, h.num_addressed_tiles);
    let _ = std::fs::remove_file(plain);
}

#[test]
fn sort_values_changes_no_decoded_tile() {
    let plain = build("unsorted", vec![extent(), lines()], &config(false, false));
    let sorted = build("sorted", vec![extent(), lines()], &config(false, true));
    let (a, b) = (tiles(&plain), tiles(&sorted));
    assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
    let mut reordered = 0;
    for (id, raw) in &b {
        assert_eq!(decoded(raw), decoded(&a[id]), "tile {id}");
        let tile = Tile::decode(raw.as_slice()).unwrap();
        for l in &tile.layers {
            let encoded: Vec<Vec<u8>> = l.values.iter().map(Value::encode_to_vec).collect();
            assert!(
                encoded.windows(2).all(|w| w[0] < w[1]),
                "values sorted and distinct"
            );
        }
        if raw != &a[id] {
            reordered += 1;
        }
    }
    assert!(reordered > 0);
    let _ = std::fs::remove_file(plain);
    let _ = std::fs::remove_file(sorted);
}

#[test]
fn sorted_layer_writes_keys_and_values_before_features() {
    let sorted = build("order", vec![lines()], &config(false, true));
    for raw in tiles(&sorted).values() {
        // Walk the first layer's fields and record their order.
        let (len, mut i) = varint(raw, 1);
        let end = i + len as usize;
        let mut order = Vec::new();
        while i < end {
            let (key, next) = varint(raw, i);
            i = next;
            let (field, wire) = (key >> 3, key & 7);
            order.push(field);
            i = match wire {
                0 => varint(raw, i).1,
                2 => {
                    let (l, n) = varint(raw, i);
                    n + l as usize
                }
                _ => unreachable!(),
            };
        }
        let first_feature = order.iter().position(|&f| f == 2).unwrap();
        assert!(order[first_feature..].iter().all(|&f| f == 2), "{order:?}");
        assert!(order[..first_feature].contains(&3) && order[..first_feature].contains(&4));
    }
    let _ = std::fs::remove_file(sorted);
}

fn varint(b: &[u8], mut i: usize) -> (u64, usize) {
    let (mut v, mut s) = (0u64, 0);
    loop {
        let c = b[i];
        i += 1;
        v |= u64::from(c & 0x7f) << s;
        s += 7;
        if c < 0x80 {
            return (v, i);
        }
    }
}

#[test]
fn line_simplify_keeps_every_feature_and_drops_vertices() {
    let base = build("lines_base", vec![lines()], &config(false, false));
    let mut opted = lines();
    opted.line_simplify = Some(1.0);
    let simplified = build("lines_dp", vec![opted], &config(false, false));
    let (a, b) = (ids(&base), ids(&simplified));
    for (key, before) in &a {
        let after = b.get(key).cloned().unwrap_or_default();
        assert!(
            before.is_subset(&after),
            "{key:?} lost {:?}",
            before.difference(&after)
        );
    }
    assert!(vertices(&simplified, "breaklines") < vertices(&base, "breaklines"));
    let _ = std::fs::remove_file(base);
    let _ = std::fs::remove_file(simplified);
}

#[test]
fn line_simplify_with_keep_short_lines_is_complete() {
    let mut opted = lines();
    opted.line_simplify = Some(1.0);
    opted.keep_short_lines = true;
    let n = opted.features.len();
    let out = build("lines_complete", vec![opted], &config(true, true));
    let per_zoom = ids(&out);
    for z in MIN_ZOOM..=MAX_ZOOM {
        let got = per_zoom
            .get(&("breaklines".to_string(), z))
            .map_or(0, BTreeSet::len);
        assert_eq!(got, n, "zoom {z}");
    }
    let _ = std::fs::remove_file(out);
}

#[test]
fn line_simplify_leaves_polygons_alone() {
    let base = build("poly_base", vec![extent()], &config(false, false));
    let mut opted = extent();
    opted.line_simplify = Some(4.0);
    let same = build("poly_opted", vec![opted], &config(false, false));
    assert_eq!(std::fs::read(&base).unwrap(), std::fs::read(&same).unwrap());
    let _ = std::fs::remove_file(base);
    let _ = std::fs::remove_file(same);
}

#[test]
fn out_of_range_line_simplify_refuses() {
    for bad in [0.0, -1.0, 64.5, f64::NAN] {
        let mut l = lines();
        l.line_simplify = Some(bad);
        let out = temp("bad");
        let err = generate_pmtiles(
            &[l],
            out.to_str().unwrap(),
            &config(false, false),
            &SilentReporter,
        )
        .unwrap_err();
        assert!(err.contains("line_simplify"), "{err}");
        assert!(!out.exists());
    }
}
