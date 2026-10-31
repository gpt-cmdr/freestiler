//! A short, nearly closed line must not vanish at low zoom. Visvalingam-Whyatt
//! pre-simplification can reduce it to its two end points, which then share a
//! pixel; the engine falls back to the original line instead of dropping it.

use freestiler_core::{
    engine::{generate_pmtiles, SilentReporter, TileConfig},
    geo_types::LineString,
    mvt::Tile,
    pmtiles_writer::TileFormat,
    tiler::{Feature, Geometry, LayerData},
};
use prost::Message;
use std::collections::BTreeSet;

#[test]
fn nearly_closed_line_survives_every_zoom() {
    // A 20-vertex loop about 6 z8 pixels across whose ends are ~0.1 px apart
    // (shape of a HEC-RAS breakline in Bald Eagle that main dropped at z8).
    let (cx, cy, r) = (-77.471, 41.119, 0.002);
    let n = 20;
    let pts: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let a = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64) * 0.999;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect();
    let layer = LayerData {
        name: "breaklines".into(),
        features: vec![Feature {
            id: Some(1),
            geometry: Geometry::LineString(LineString::from(pts)),
            properties: vec![],
        }],
        prop_names: vec![],
        prop_types: vec![],
        min_zoom: 4,
        max_zoom: 14,
    };
    let config = TileConfig {
        tile_format: TileFormat::Mvt,
        min_zoom: 4,
        max_zoom: 14,
        base_zoom: None,
        simplification: true,
        drop_rate: None,
        cluster_distance: None,
        cluster_maxzoom: None,
        coalesce: false,
        gzip_level: None,
        buffer: None,
    };
    let out = std::env::temp_dir().join(format!("freestiler_line_{}.pmtiles", std::process::id()));
    generate_pmtiles(&[layer], out.to_str().unwrap(), &config, &SilentReporter).unwrap();

    use std::io::Read;
    let mut archive = pmtiles2::PMTiles::from_reader(std::fs::File::open(&out).unwrap()).unwrap();
    let ids: Vec<u64> = archive.tile_ids().into_iter().copied().collect();
    let mut zooms = BTreeSet::new();
    for id in ids {
        let z = (0..32u8)
            .find(|&z| pmtiles2::util::tile_id(z + 1, 0, 0) > id)
            .unwrap();
        let gz = archive.get_tile_by_id(id).unwrap().unwrap();
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        if Tile::decode(raw.as_slice())
            .unwrap()
            .layers
            .iter()
            .any(|l| !l.features.is_empty())
        {
            zooms.insert(z);
        }
    }
    let _ = std::fs::remove_file(&out);
    // At z4-z6 the whole loop is under one pixel; from z7 it spans pixels.
    for z in 7..=14 {
        assert!(zooms.contains(&z), "line missing at z{z}: {zooms:?}");
    }
}
