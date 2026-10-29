//! `TileConfig::buffer` narrows the clip buffer without dropping features:
//! every feature stays at every zoom, archives shrink, and out-of-range
//! buffers refuse.

use freestiler_core::{
    engine::{generate_pmtiles, SilentReporter, TileConfig},
    geo_types::{LineString, Polygon},
    mvt::Tile,
    pmtiles_writer::TileFormat,
    tiler::{Feature, Geometry, LayerData, PropertyValue},
};
use prost::Message;
use std::collections::{BTreeMap, BTreeSet};

const N: usize = 40;

fn mesh() -> LayerData {
    let step = 0.002;
    let mut features = Vec::new();
    for i in 0..N {
        for j in 0..N {
            let (x, y) = (-77.9 + i as f64 * step, 40.9 + j as f64 * step);
            let ring = LineString::from(vec![
                (x, y),
                (x + step, y),
                (x + step, y + step),
                (x, y + step),
                (x, y),
            ]);
            let k = (i * N + j) as i64;
            features.push(Feature {
                id: Some(k as u64 + 1),
                geometry: Geometry::Polygon(Polygon::new(ring, vec![])),
                properties: vec![PropertyValue::Int(k)],
            });
        }
    }
    LayerData {
        name: "cells".into(),
        features,
        prop_names: vec!["cell_id".into()],
        prop_types: vec!["integer".into()],
        min_zoom: 9,
        max_zoom: 15,
    }
}

fn config(buffer: Option<f64>) -> TileConfig {
    TileConfig {
        tile_format: TileFormat::Mvt,
        min_zoom: 9,
        max_zoom: 15,
        base_zoom: None,
        simplification: true,
        drop_rate: None,
        cluster_distance: None,
        cluster_maxzoom: None,
        coalesce: false,
        gzip_level: None,
        buffer,
    }
}

/// Feature IDs per zoom, and the total encoded feature count.
fn ids_per_zoom(path: &std::path::Path) -> (BTreeMap<u8, BTreeSet<u64>>, usize) {
    use std::io::Read;
    let mut archive = pmtiles2::PMTiles::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    let ids: Vec<u64> = archive.tile_ids().into_iter().copied().collect();
    let mut per_zoom: BTreeMap<u8, BTreeSet<u64>> = BTreeMap::new();
    let mut total = 0;
    for id in ids {
        let z = (0..32u8)
            .find(|&z| pmtiles2::util::tile_id(z + 1, 0, 0) > id)
            .unwrap();
        let gz = archive.get_tile_by_id(id).unwrap().unwrap();
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut raw)
            .unwrap();
        for layer in Tile::decode(raw.as_slice()).unwrap().layers {
            total += layer.features.len();
            per_zoom
                .entry(z)
                .or_default()
                .extend(layer.features.iter().filter_map(|f| f.id));
        }
    }
    (per_zoom, total)
}

#[test]
fn narrow_buffer_keeps_every_feature_and_shrinks() {
    let dir = std::env::temp_dir().join(format!("freestiler_buffer_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir(&dir).unwrap();
    let wide = dir.join("wide.pmtiles");
    let narrow = dir.join("narrow.pmtiles");
    generate_pmtiles(
        &[mesh()],
        wide.to_str().unwrap(),
        &config(None),
        &SilentReporter,
    )
    .unwrap();
    generate_pmtiles(
        &[mesh()],
        narrow.to_str().unwrap(),
        &config(Some(5.0)),
        &SilentReporter,
    )
    .unwrap();
    let (wide_ids, wide_total) = ids_per_zoom(&wide);
    let (narrow_ids, narrow_total) = ids_per_zoom(&narrow);
    assert_eq!(wide_ids.len(), 7);
    for (z, ids) in &narrow_ids {
        assert_eq!(ids.len(), N * N, "zoom {z} lost features with buffer 5");
    }
    assert_eq!(wide_ids, narrow_ids);
    assert!(narrow_total < wide_total, "{narrow_total} >= {wide_total}");
    assert!(std::fs::metadata(&narrow).unwrap().len() < std::fs::metadata(&wide).unwrap().len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn out_of_range_buffer_refuses() {
    let out = std::env::temp_dir().join(format!(
        "freestiler_buffer_bad_{}.pmtiles",
        std::process::id()
    ));
    for bad in [-1.0, 128.5, f64::NAN] {
        let err = generate_pmtiles(
            &[mesh()],
            out.to_str().unwrap(),
            &config(Some(bad)),
            &SilentReporter,
        )
        .unwrap_err();
        assert!(err.contains("buffer"), "{err}");
    }
}
