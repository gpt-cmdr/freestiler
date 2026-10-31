//! Arrow / GeoArrow input: build a [`LayerData`] from Arrow record batches,
//! reprojecting to longitude/latitude in pure Rust.
//!
//! The geometry column may be WKB (`Binary`, `LargeBinary`, `BinaryView`,
//! with or without the `geoarrow.wkb` extension) or native GeoArrow
//! (`geoarrow.point`, `.linestring`, `.polygon`, `.multipoint`,
//! `.multilinestring`, `.multipolygon`) with separated (struct) or
//! interleaved (fixed-size list) coordinates. Z/M ordinates are ignored.
//!
//! The CRS is taken, in order, from the caller's override, the geometry
//! field's GeoArrow extension metadata, or the GeoParquet `geo` schema
//! metadata (where an absent `crs` key means OGC:CRS84, per the GeoParquet
//! specification). Input whose CRS is unknown is refused: tiling projected
//! coordinates as if they were degrees produces a valid but silently wrong
//! tileset.

use arrow_array::cast::AsArray;
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, SchemaRef};
use geo_types::{Coord, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use rayon::prelude::*;
use serde_json::Value;

use crate::crs::{self, SourceCrs};
use crate::file_input::{arrow_type_to_string, extract_property_value, wkb_to_geometry};
use crate::tiler::{Feature, Geometry, LayerData, PropertyValue};

/// Options for [`batches_to_layer`].
pub struct ArrowLayerOptions {
    pub name: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Geometry column; `None` picks the GeoArrow-typed field, then the
    /// GeoParquet primary column, then `geometry`.
    pub geometry_column: Option<String>,
    /// CRS override: PROJJSON, a PROJ string, or `EPSG:4326` / `OGC:CRS84`.
    pub crs: Option<String>,
    /// First feature ID minus one; `None` writes no IDs.
    pub id_offset: Option<u64>,
}

/// Summary of an Arrow layer conversion.
#[derive(Debug)]
pub struct ArrowLayerReport {
    pub rows: usize,
    pub features: usize,
    pub skipped_null_or_empty: usize,
    pub skipped_columns: Vec<String>,
    /// The PROJ string used for reprojection, `None` when the input was
    /// already longitude/latitude, `"web-mercator"` for EPSG:3857.
    pub reprojection: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GeomKind {
    Wkb,
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

fn extension_name(field: &Field) -> Option<&str> {
    field
        .metadata()
        .get("ARROW:extension:name")
        .map(|s| s.as_str())
}

fn geom_kind(field: &Field) -> Result<GeomKind, String> {
    match extension_name(field) {
        Some("geoarrow.wkb") | Some("ogc.wkb") => Ok(GeomKind::Wkb),
        Some("geoarrow.point") => Ok(GeomKind::Point),
        Some("geoarrow.linestring") => Ok(GeomKind::LineString),
        Some("geoarrow.polygon") => Ok(GeomKind::Polygon),
        Some("geoarrow.multipoint") => Ok(GeomKind::MultiPoint),
        Some("geoarrow.multilinestring") => Ok(GeomKind::MultiLineString),
        Some("geoarrow.multipolygon") => Ok(GeomKind::MultiPolygon),
        Some(other) if other.starts_with("geoarrow.") => Err(format!(
            "geometry column '{}' has unsupported GeoArrow type {other}",
            field.name()
        )),
        _ => match field.data_type() {
            DataType::Binary | DataType::LargeBinary | DataType::BinaryView => Ok(GeomKind::Wkb),
            other => Err(format!(
                "geometry column '{}' is {other:?}, not WKB or a GeoArrow extension type",
                field.name()
            )),
        },
    }
}

fn geo_metadata(schema: &SchemaRef) -> Option<Value> {
    schema
        .metadata()
        .get("geo")
        .and_then(|s| serde_json::from_str(s).ok())
}

/// Pick the geometry column index.
fn find_geometry(schema: &SchemaRef, requested: Option<&str>) -> Result<usize, String> {
    if let Some(name) = requested {
        return schema
            .index_of(name)
            .map_err(|_| format!("geometry column '{name}' not found"));
    }
    if let Some(i) = schema.fields().iter().position(|f| {
        extension_name(f).is_some_and(|n| n.starts_with("geoarrow.") || n == "ogc.wkb")
    }) {
        return Ok(i);
    }
    if let Some(primary) = geo_metadata(schema)
        .as_ref()
        .and_then(|g| g.get("primary_column"))
        .and_then(|p| p.as_str())
    {
        if let Ok(i) = schema.index_of(primary) {
            return Ok(i);
        }
    }
    schema
        .index_of("geometry")
        .map_err(|_| "no geometry column: name one with geometry_column".to_string())
}

/// The CRS definition carried by the input, or an error when it is unknown.
fn input_crs(schema: &SchemaRef, field: &Field) -> Result<String, String> {
    if let Some(meta) = field.metadata().get("ARROW:extension:metadata") {
        if !meta.trim().is_empty() {
            let v: Value = serde_json::from_str(meta)
                .map_err(|e| format!("invalid GeoArrow extension metadata: {e}"))?;
            match v.get("crs") {
                Some(Value::Object(_)) => return Ok(v["crs"].to_string()),
                Some(Value::String(s)) => {
                    let s = s.trim();
                    if s.starts_with('{')
                        || s.starts_with('+')
                        || s.contains(':') && !s.contains('[')
                    {
                        return Ok(s.to_string());
                    }
                    return Err(format!(
                        "geometry column '{}' carries a {} CRS freestiler cannot read; pass crs= \
                         as PROJJSON (pyproj CRS.to_json())",
                        field.name(),
                        v.get("crs_type").and_then(|t| t.as_str()).unwrap_or("WKT")
                    ));
                }
                Some(Value::Null) | None => {}
                Some(other) => return Err(format!("unsupported GeoArrow crs value {other}")),
            }
        }
    }
    if let Some(geo) = geo_metadata(schema) {
        if let Some(col) = geo.get("columns").and_then(|c| c.get(field.name())) {
            return match col.get("crs") {
                // GeoParquet: an absent crs key means OGC:CRS84.
                None => Ok("OGC:CRS84".to_string()),
                Some(Value::Null) => Err(format!(
                    "geometry column '{}' has an explicitly unknown CRS (GeoParquet crs: null); \
                     pass crs=",
                    field.name()
                )),
                Some(Value::String(s)) => Ok(s.clone()),
                Some(v) => Ok(v.to_string()),
            };
        }
    }
    Err(format!(
        "geometry column '{}' has no CRS; refusing to tile coordinates of unknown meaning. \
         Pass crs= (PROJJSON, a PROJ string, or EPSG:4326)",
        field.name()
    ))
}

/// Convert Arrow record batches into one layer, reprojected to EPSG:4326.
pub fn batches_to_layer<I>(
    schema: SchemaRef,
    batches: I,
    opts: &ArrowLayerOptions,
) -> Result<(LayerData, ArrowLayerReport), String>
where
    I: IntoIterator<Item = Result<RecordBatch, String>>,
{
    let geom_idx = find_geometry(&schema, opts.geometry_column.as_deref())?;
    let geom_field = schema.field(geom_idx).clone();
    let kind = geom_kind(&geom_field)?;
    let crs_def = match &opts.crs {
        Some(c) => c.clone(),
        None => input_crs(&schema, &geom_field)?,
    };
    let source: SourceCrs = crs::resolve(&crs_def)?;

    let mut prop_cols = Vec::new();
    let mut prop_names = Vec::new();
    let mut prop_types = Vec::new();
    let mut skipped_columns = Vec::new();
    for (i, f) in schema.fields().iter().enumerate() {
        if i == geom_idx {
            continue;
        }
        if is_scalar(f.data_type()) {
            prop_cols.push(i);
            prop_names.push(f.name().clone());
            prop_types.push(arrow_type_to_string(f.data_type()));
        } else {
            skipped_columns.push(f.name().clone());
        }
    }

    let mut features: Vec<Feature> = Vec::new();
    let mut rows = 0usize;
    let mut skipped = 0usize;
    for batch in batches {
        let batch = batch?;
        if batch.schema().fields().len() != schema.fields().len() {
            return Err("record batch schema differs from the stream schema".into());
        }
        let geom = batch.column(geom_idx);
        let mut batch_feats = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            rows += 1;
            let Some(geometry) = decode(geom, kind, row)? else {
                skipped += 1;
                continue;
            };
            let properties = prop_cols
                .iter()
                .map(|&c| extract_property_value(batch.column(c).as_ref(), row))
                .collect::<Result<Vec<PropertyValue>, String>>()?;
            batch_feats.push(Feature {
                id: None,
                geometry,
                properties,
            });
        }
        if !source.is_identity() {
            batch_feats
                .par_iter_mut()
                .try_for_each(|f| source.to_lonlat(&mut f.geometry))?;
        }
        features.append(&mut batch_feats);
    }
    if let Some(offset) = opts.id_offset {
        for (i, f) in features.iter_mut().enumerate() {
            f.id = Some(offset + i as u64 + 1);
        }
    }
    let report = ArrowLayerReport {
        rows,
        features: features.len(),
        skipped_null_or_empty: skipped,
        skipped_columns,
        reprojection: match &source {
            SourceCrs::LonLat => None,
            SourceCrs::WebMercator => Some("web-mercator".into()),
            SourceCrs::Proj(..) => source.proj_string().map(str::to_string),
        },
    };
    Ok((
        LayerData {
            name: opts.name.clone(),
            features,
            prop_names,
            prop_types,
            min_zoom: opts.min_zoom,
            max_zoom: opts.max_zoom,
        },
        report,
    ))
}

fn is_scalar(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
            | DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Date32
            | DataType::Date64
            | DataType::Timestamp(_, _)
            | DataType::Time32(_)
            | DataType::Time64(_)
    )
}

fn decode(col: &ArrayRef, kind: GeomKind, row: usize) -> Result<Option<Geometry>, String> {
    if col.is_null(row) {
        return Ok(None);
    }
    let g = match kind {
        GeomKind::Wkb => {
            let bytes: &[u8] = match col.data_type() {
                DataType::Binary => col.as_binary::<i32>().value(row),
                DataType::LargeBinary => col.as_binary::<i64>().value(row),
                DataType::BinaryView => col.as_binary_view().value(row),
                other => return Err(format!("WKB column has storage type {other:?}")),
            };
            match wkb_to_geometry(bytes) {
                Some(g) => g,
                None => {
                    // Empty geometries and GeometryCollections decode to None.
                    return Ok(None);
                }
            }
        }
        GeomKind::Point => {
            let c = coord(col.as_ref(), row)?;
            if c.x.is_nan() || c.y.is_nan() {
                return Ok(None);
            }
            Geometry::Point(Point(c))
        }
        GeomKind::LineString => {
            Geometry::LineString(LineString(coords(list_child(col.as_ref(), row)?.as_ref())?))
        }
        GeomKind::Polygon => Geometry::Polygon(polygon(list_child(col.as_ref(), row)?.as_ref())?),
        GeomKind::MultiPoint => Geometry::MultiPoint(MultiPoint(
            coords(list_child(col.as_ref(), row)?.as_ref())?
                .into_iter()
                .map(Point)
                .collect(),
        )),
        GeomKind::MultiLineString => {
            let parts = list_child(col.as_ref(), row)?;
            let mut out = Vec::with_capacity(parts.len());
            for i in 0..parts.len() {
                out.push(LineString(coords(list_child(parts.as_ref(), i)?.as_ref())?));
            }
            Geometry::MultiLineString(MultiLineString(out))
        }
        GeomKind::MultiPolygon => {
            let polys = list_child(col.as_ref(), row)?;
            let mut out = Vec::with_capacity(polys.len());
            for i in 0..polys.len() {
                out.push(polygon(list_child(polys.as_ref(), i)?.as_ref())?);
            }
            Geometry::MultiPolygon(MultiPolygon(out))
        }
    };
    Ok(if is_empty(&g) { None } else { Some(g) })
}

fn is_empty(g: &Geometry) -> bool {
    match g {
        Geometry::Point(_) => false,
        Geometry::MultiPoint(m) => m.0.is_empty(),
        Geometry::LineString(l) => l.0.is_empty(),
        Geometry::MultiLineString(m) => m.0.iter().all(|l| l.0.is_empty()),
        Geometry::Polygon(p) => p.exterior().0.is_empty(),
        Geometry::MultiPolygon(m) => m.0.iter().all(|p| p.exterior().0.is_empty()),
    }
}

fn list_child(arr: &dyn Array, i: usize) -> Result<ArrayRef, String> {
    match arr.data_type() {
        DataType::List(_) => Ok(arr.as_list::<i32>().value(i)),
        DataType::LargeList(_) => Ok(arr.as_list::<i64>().value(i)),
        other => Err(format!("GeoArrow nesting must be a List, found {other:?}")),
    }
}

fn polygon(rings: &dyn Array) -> Result<Polygon<f64>, String> {
    let mut out = Vec::with_capacity(rings.len());
    for i in 0..rings.len() {
        out.push(LineString(coords(list_child(rings, i)?.as_ref())?));
    }
    let mut it = out.into_iter();
    let exterior = it.next().unwrap_or_else(|| LineString(vec![]));
    Ok(Polygon::new(exterior, it.collect()))
}

/// All coordinates of a GeoArrow coordinate array (struct or interleaved).
fn coords(arr: &dyn Array) -> Result<Vec<Coord<f64>>, String> {
    (0..arr.len()).map(|i| coord(arr, i)).collect()
}

fn f64_at(arr: &dyn Array, i: usize) -> Result<f64, String> {
    match arr.data_type() {
        DataType::Float64 => Ok(arr
            .as_primitive::<arrow_array::types::Float64Type>()
            .value(i)),
        DataType::Float32 => Ok(arr
            .as_primitive::<arrow_array::types::Float32Type>()
            .value(i) as f64),
        other => Err(format!(
            "GeoArrow coordinates must be floating point, found {other:?}"
        )),
    }
}

fn coord(arr: &dyn Array, i: usize) -> Result<Coord<f64>, String> {
    match arr.data_type() {
        DataType::Struct(_) => {
            let s = arr.as_struct();
            if s.num_columns() < 2 {
                return Err("GeoArrow struct coordinates need x and y".into());
            }
            Ok(Coord {
                x: f64_at(s.column(0).as_ref(), i)?,
                y: f64_at(s.column(1).as_ref(), i)?,
            })
        }
        DataType::FixedSizeList(_, n) if *n >= 2 => {
            let l = arr.as_fixed_size_list();
            let values = l.values();
            let base = i * *n as usize;
            Ok(Coord {
                x: f64_at(values.as_ref(), base)?,
                y: f64_at(values.as_ref(), base + 1)?,
            })
        }
        other => Err(format!("unsupported GeoArrow coordinate type {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{BinaryBuilder, Float64Builder};
    use arrow_array::{Float64Array, Int64Array};
    use arrow_schema::Schema;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn wkb_point(x: f64, y: f64) -> Vec<u8> {
        let mut b = vec![1u8, 1, 0, 0, 0];
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
        b
    }

    fn table(
        meta: HashMap<String, String>,
        field_meta: HashMap<String, String>,
    ) -> (SchemaRef, RecordBatch) {
        let mut g = BinaryBuilder::new();
        g.append_value(wkb_point(2_003_437.560527763, 323_626.1972692954));
        g.append_null();
        let mut v = Float64Builder::new();
        v.append_value(1.5);
        v.append_value(2.5);
        let schema = Arc::new(Schema::new_with_metadata(
            vec![
                Field::new("cell_id", DataType::Int64, false),
                Field::new("geometry", DataType::Binary, true).with_metadata(field_meta),
                Field::new("value", DataType::Float64, false),
            ],
            meta,
        ));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![7, 8])),
                Arc::new(g.finish()),
                Arc::new(Float64Array::from(vec![1.5, 2.5])),
            ],
        )
        .unwrap();
        let _ = v;
        (schema, batch)
    }

    fn opts(crs: Option<&str>) -> ArrowLayerOptions {
        ArrowLayerOptions {
            name: "cells".into(),
            min_zoom: 0,
            max_zoom: 14,
            geometry_column: None,
            crs: crs.map(str::to_string),
            id_offset: Some(10),
        }
    }

    #[test]
    fn refuses_input_without_crs() {
        let (schema, batch) = table(HashMap::new(), HashMap::new());
        let err = batches_to_layer(schema, vec![Ok(batch)], &opts(None))
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("no CRS"), "{err}");
    }

    #[test]
    fn geoparquet_metadata_without_crs_key_is_crs84() {
        let meta = HashMap::from([(
            "geo".to_string(),
            r#"{"primary_column":"geometry","columns":{"geometry":{"encoding":"WKB"}}}"#
                .to_string(),
        )]);
        let (schema, batch) = table(meta, HashMap::new());
        let (layer, report) = batches_to_layer(schema, vec![Ok(batch)], &opts(None)).unwrap();
        assert_eq!(report.reprojection, None);
        assert_eq!(layer.features.len(), 1);
        assert_eq!(report.skipped_null_or_empty, 1);
    }

    #[test]
    fn geoparquet_null_crs_is_refused() {
        let meta = HashMap::from([(
            "geo".to_string(),
            r#"{"primary_column":"geometry","columns":{"geometry":{"encoding":"WKB","crs":null}}}"#
                .to_string(),
        )]);
        let (schema, batch) = table(meta, HashMap::new());
        assert!(batches_to_layer(schema, vec![Ok(batch)], &opts(None)).is_err());
    }

    #[test]
    fn reprojects_from_override_and_keeps_properties_and_ids() {
        let (schema, batch) = table(HashMap::new(), HashMap::new());
        let pa_north = "+proj=lcc +lat_0=40.1666666666667 +lon_0=-77.75 +lat_1=41.95 +lat_2=40.8833333333333 \
                        +x_0=600000 +y_0=0 +a=6378137 +rf=298.257222101 +towgs84=0,0,0 +to_meter=0.304800609601219 +no_defs";
        let (layer, report) =
            batches_to_layer(schema, vec![Ok(batch)], &opts(Some(pa_north))).unwrap();
        assert!(report.reprojection.is_some());
        assert_eq!(layer.prop_names, vec!["cell_id", "value"]);
        let f = &layer.features[0];
        assert_eq!(f.id, Some(11));
        assert_eq!(
            f.properties,
            vec![PropertyValue::Int(7), PropertyValue::Double(1.5)]
        );
        let Geometry::Point(p) = &f.geometry else {
            panic!()
        };
        assert!((p.x() - -77.62332198510).abs() < 1e-9, "{}", p.x());
        assert!((p.y() - 41.05482932662).abs() < 1e-9, "{}", p.y());
    }

    #[test]
    fn geoarrow_extension_crs_is_used() {
        let fm = HashMap::from([
            (
                "ARROW:extension:name".to_string(),
                "geoarrow.wkb".to_string(),
            ),
            (
                "ARROW:extension:metadata".to_string(),
                r#"{"crs":"EPSG:3857"}"#.to_string(),
            ),
        ]);
        let (schema, batch) = table(HashMap::new(), fm);
        let (_, report) = batches_to_layer(schema, vec![Ok(batch)], &opts(None)).unwrap();
        assert_eq!(report.reprojection.as_deref(), Some("web-mercator"));
    }
}
