//! Source CRS resolution and reprojection to EPSG:4326 in pure Rust.
//!
//! Vector tiles are built from longitude/latitude. Input in another CRS is
//! reprojected here with [`proj4rs`], so callers can hand over projected
//! data (for example a HEC-RAS mesh in a US survey foot state plane) without
//! a Python or C reprojection step.
//!
//! The CRS arrives as PROJJSON (what GeoParquet and GeoArrow metadata carry),
//! a PROJ string, or an EPSG:4326 / OGC:CRS84 alias. PROJJSON is converted to
//! a PROJ string here. The conversion never guesses:
//!
//! - Only the projection methods in [`METHODS`] are accepted; anything else
//!   is refused with the method name.
//! - Linear units come from the coordinate system's conversion factor, so a
//!   US survey foot CRS stays in US survey feet (never silently metres).
//! - Datums whose default PROJ operation to WGS 84 is a null transformation
//!   (WGS 84, NAD83, NAD83(NSRS2007), NAD83(2011), ETRS89, GDA94, GDA2020,
//!   NZGD2000, SIRGAS 2000) are reprojected without a datum shift, exactly as
//!   PROJ does. NAD83(HARN) and NAD83(CSRS), which PROJ shifts by about a
//!   metre, are refused. A `BoundCRS` to WGS 84 with a 3- or 7-term
//!   Helmert transformation is applied as `+towgs84`. Every other datum
//!   (NAD27, ED50, OSGB36, ...) needs a grid or an unknown shift and is
//!   refused rather than guessed.

use proj4rs::proj::Proj;
use serde_json::Value;

use crate::tiler::Geometry;

/// Projection methods understood by the PROJJSON converter, by EPSG method
/// code, with the PROJ projection name.
pub const METHODS: &[(u32, &str, &str)] = &[
    (9807, "Transverse Mercator", "tmerc"),
    (9801, "Lambert Conic Conformal (1SP)", "lcc"),
    (9802, "Lambert Conic Conformal (2SP)", "lcc"),
    (9822, "Albers Equal Area", "aea"),
    (9804, "Mercator (variant A)", "merc"),
    (9805, "Mercator (variant B)", "merc"),
    (1024, "Popular Visualisation Pseudo Mercator", "merc"),
    (9809, "Oblique Stereographic", "sterea"),
    (9820, "Lambert Azimuthal Equal Area", "laea"),
];

/// Datum names (lower case, punctuation removed) that PROJ treats as
/// coincident with WGS 84 when no grid is available.
const WGS84_COMPATIBLE: &[&str] = &[
    // Matched exactly against the normalized datum name, except the WGS 84
    // and ETRS89 entries, which also cover their ensembles and realizations.
    // Each was checked against pyproj 3.8 / PROJ 9.8: its default operation
    // to WGS 84 is a null transformation. NAD83(HARN) and NAD83(CSRS) are
    // deliberately absent: PROJ shifts them by about a metre.
    "worldgeodeticsystem1984*",
    "europeanterrestrialreferencesystem1989*",
    "northamericandatum1983",
    "nad83nationalspatialreferencesystem2007",
    "nad83nationalspatialreferencesystem2011",
    "geocentricdatumofaustralia1994",
    "geocentricdatumofaustralia2020",
    "newzealandgeodeticdatum2000",
    "sistemadereferenciageocentricoparalasamericas2000",
];

/// How coordinates in the source CRS reach longitude/latitude.
pub enum SourceCrs {
    /// Already longitude/latitude on a WGS 84 compatible datum.
    LonLat,
    /// Spherical Web Mercator (EPSG:3857), inverted in closed form.
    WebMercator,
    /// Any other supported CRS, inverted with proj4rs.
    Proj(Box<Proj>, Box<Proj>, String),
}

impl std::fmt::Debug for SourceCrs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceCrs::LonLat => write!(f, "LonLat"),
            SourceCrs::WebMercator => write!(f, "WebMercator"),
            SourceCrs::Proj(_, _, s) => write!(f, "Proj({s})"),
        }
    }
}

impl SourceCrs {
    /// The PROJ string used for the inverse, if any (for provenance and tests).
    pub fn proj_string(&self) -> Option<&str> {
        match self {
            SourceCrs::Proj(_, _, s) => Some(s),
            _ => None,
        }
    }

    /// True when coordinates are used as they are.
    pub fn is_identity(&self) -> bool {
        matches!(self, SourceCrs::LonLat)
    }

    /// Reproject one geometry to longitude/latitude degrees in place.
    pub fn to_lonlat(&self, geom: &mut Geometry) -> Result<(), String> {
        match self {
            SourceCrs::LonLat => Ok(()),
            SourceCrs::WebMercator => {
                const R: f64 = 6_378_137.0;
                map_geometry(geom, &mut |c| {
                    c.x = (c.x / R).to_degrees();
                    c.y = (c.y / R).sinh().atan().to_degrees();
                    Ok(())
                })
            }
            SourceCrs::Proj(src, dst, _) => map_geometry(geom, &mut |c| {
                let mut p = (c.x, c.y, 0.0);
                proj4rs::transform::transform(src, dst, &mut p)
                    .map_err(|e| format!("cannot reproject ({}, {}): {e}", c.x, c.y))?;
                if !p.0.is_finite() || !p.1.is_finite() {
                    return Err(format!("cannot reproject ({}, {}): not finite", c.x, c.y));
                }
                c.x = p.0.to_degrees();
                c.y = p.1.to_degrees();
                Ok(())
            }),
        }
    }
}

fn map_geometry(
    geom: &mut Geometry,
    f: &mut dyn FnMut(&mut geo_types::Coord<f64>) -> Result<(), String>,
) -> Result<(), String> {
    fn ls(
        l: &mut geo_types::LineString<f64>,
        f: &mut dyn FnMut(&mut geo_types::Coord<f64>) -> Result<(), String>,
    ) -> Result<(), String> {
        l.0.iter_mut().try_for_each(&mut *f)
    }
    fn poly(
        p: &mut geo_types::Polygon<f64>,
        f: &mut dyn FnMut(&mut geo_types::Coord<f64>) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut err = Ok(());
        p.exterior_mut(|e| err = ls(e, f));
        err?;
        let mut err = Ok(());
        p.interiors_mut(|rings| {
            for r in rings {
                if let Err(e) = ls(r, f) {
                    err = Err(e);
                    return;
                }
            }
        });
        err
    }
    match geom {
        Geometry::Point(p) => f(&mut p.0),
        Geometry::MultiPoint(mp) => mp.0.iter_mut().try_for_each(|p| f(&mut p.0)),
        Geometry::LineString(l) => ls(l, f),
        Geometry::MultiLineString(m) => m.0.iter_mut().try_for_each(|l| ls(l, f)),
        Geometry::Polygon(p) => poly(p, f),
        Geometry::MultiPolygon(m) => m.0.iter_mut().try_for_each(|p| poly(p, f)),
    }
}

/// Resolve a CRS definition: PROJJSON text, a PROJ string (`+proj=...`), or
/// one of `EPSG:4326`, `OGC:CRS84`, `CRS84`, `WGS84`, `EPSG:3857`.
pub fn resolve(definition: &str) -> Result<SourceCrs, String> {
    let def = definition.trim();
    if def.is_empty() {
        return Err("empty CRS definition".into());
    }
    let upper = def.to_ascii_uppercase();
    match upper.as_str() {
        "EPSG:4326" | "OGC:CRS84" | "CRS84" | "WGS84" | "OGC:4326" | "EPSG:4269" | "OGC:CRS83" => {
            return Ok(SourceCrs::LonLat)
        }
        "EPSG:3857" | "EPSG:900913" => return Ok(SourceCrs::WebMercator),
        _ => {}
    }
    if def.starts_with('{') {
        let v: Value =
            serde_json::from_str(def).map_err(|e| format!("CRS is not valid PROJJSON: {e}"))?;
        return resolve_projjson(&v);
    }
    if def.starts_with('+') || def.starts_with("proj=") {
        return proj_from_string(def);
    }
    Err(format!(
        "unsupported CRS definition {:?}: pass PROJJSON (pyproj CRS.to_json()), a PROJ string, \
         or EPSG:4326",
        truncate(def, 80)
    ))
}

/// Resolve a parsed PROJJSON object.
pub fn resolve_projjson(v: &Value) -> Result<SourceCrs, String> {
    if let Some((auth, code)) = id_of(v) {
        match (auth.as_str(), code.as_str()) {
            ("EPSG", "4326") | ("OGC", "CRS84") | ("EPSG", "4269") | ("OGC", "CRS83") => {
                return Ok(SourceCrs::LonLat)
            }
            ("EPSG", "3857") => return Ok(SourceCrs::WebMercator),
            _ => {}
        }
    }
    let s = projjson_to_proj_string(v)?;
    if s == LONLAT {
        return Ok(SourceCrs::LonLat);
    }
    if s == WEB_MERCATOR {
        return Ok(SourceCrs::WebMercator);
    }
    proj_from_string(&s)
}

const LONLAT: &str = "+proj=longlat +datum=WGS84 +no_defs";
const WEB_MERCATOR: &str = "web-mercator";

fn proj_from_string(s: &str) -> Result<SourceCrs, String> {
    let src =
        Proj::from_proj_string(s).map_err(|e| format!("cannot use PROJ string {s:?}: {e}"))?;
    if !src.has_inverse() {
        return Err(format!("projection in {s:?} has no inverse"));
    }
    let dst = Proj::from_proj_string(LONLAT).map_err(|e| e.to_string())?;
    Ok(SourceCrs::Proj(Box::new(src), Box::new(dst), s.to_string()))
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(n).collect::<String>())
    }
}

fn id_of(v: &Value) -> Option<(String, String)> {
    let id = v
        .get("id")
        .or_else(|| v.get("ids").and_then(|i| i.get(0)))?;
    let auth = id.get("authority")?.as_str()?.to_ascii_uppercase();
    let code = match id.get("code")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    Some((auth, code))
}

fn name_of(v: &Value) -> String {
    v.get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string()
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Conversion factor of a PROJJSON unit (to metres or to radians).
fn unit_factor(unit: Option<&Value>, default: f64) -> Result<f64, String> {
    match unit {
        None => Ok(default),
        Some(Value::String(s)) => match s.as_str() {
            "metre" | "meter" => Ok(1.0),
            "degree" => Ok(std::f64::consts::PI / 180.0),
            "unity" => Ok(1.0),
            "radian" => Ok(1.0),
            other => Err(format!("unknown unit {other:?}")),
        },
        Some(u) => u
            .get("conversion_factor")
            .and_then(|f| f.as_f64())
            .filter(|f| f.is_finite() && *f > 0.0)
            .ok_or_else(|| format!("unit without a conversion factor: {u}")),
    }
}

/// Convert a PROJJSON CRS to a PROJ string, refusing anything it cannot
/// represent exactly.
pub fn projjson_to_proj_string(v: &Value) -> Result<String, String> {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "BoundCRS" => {
            let source = v.get("source_crs").ok_or("BoundCRS without source_crs")?;
            let target = v.get("target_crs").ok_or("BoundCRS without target_crs")?;
            if !is_wgs84_target(target) {
                return Err(format!(
                    "BoundCRS targets {:?}, not WGS 84; reproject before tiling",
                    name_of(target)
                ));
            }
            let towgs84 = helmert_towgs84(
                v.get("transformation")
                    .ok_or("BoundCRS without transformation")?,
            )?;
            crs_to_proj(source, Some(towgs84))
        }
        _ => crs_to_proj(v, None),
    }
}

fn is_wgs84_target(target: &Value) -> bool {
    matches!(id_of(target), Some((a, c)) if a == "EPSG" && (c == "4326" || c == "4979" || c == "4978"))
        || normalize(&name_of(target)).starts_with("wgs84")
}

/// `+towgs84` terms for a Helmert transformation, in PROJ's position vector
/// convention (coordinate frame rotations are negated).
fn helmert_towgs84(t: &Value) -> Result<String, String> {
    let method = t.get("method").ok_or("transformation without method")?;
    let mname = normalize(&name_of(method));
    let code = id_of(method).map(|(_, c)| c).unwrap_or_default();
    let (seven, coordinate_frame) = match code.as_str() {
        "9603" | "1031" => (false, false),
        "9606" | "1033" => (true, false),
        "9607" | "1032" => (true, true),
        _ if mname.starts_with("geocentrictranslations") => (false, false),
        _ if mname.starts_with("positionvector") => (true, false),
        _ if mname.starts_with("coordinateframe") => (true, true),
        _ => {
            return Err(format!(
            "datum transformation {:?} is not a Helmert transformation; reproject before tiling",
            name_of(method)
        ))
        }
    };
    let params = t
        .get("parameters")
        .and_then(|p| p.as_array())
        .ok_or("transformation without parameters")?;
    let get = |codes: &[&str], names: &[&str], kind: char| -> Result<f64, String> {
        for p in params {
            let pc = id_of(p).map(|(_, c)| c).unwrap_or_default();
            let pn = normalize(&name_of(p));
            if codes.contains(&pc.as_str()) || names.iter().any(|n| pn == *n) {
                let value = p
                    .get("value")
                    .and_then(|x| x.as_f64())
                    .ok_or("parameter without value")?;
                let unit = p.get("unit");
                return Ok(match kind {
                    // metres
                    'l' => value * unit_factor(unit, 1.0)?,
                    // arc-seconds
                    'a' => {
                        let rad = value * unit_factor(unit, std::f64::consts::PI / 648_000.0)?;
                        rad * 648_000.0 / std::f64::consts::PI
                    }
                    // parts per million
                    _ => {
                        let unity = value * unit_factor(unit, 1e-6)?;
                        unity * 1e6
                    }
                });
            }
        }
        Err(format!("Helmert parameter {:?} missing", names[0]))
    };
    let dx = get(&["8605"], &["xaxistranslation"], 'l')?;
    let dy = get(&["8606"], &["yaxistranslation"], 'l')?;
    let dz = get(&["8607"], &["zaxistranslation"], 'l')?;
    if !seven {
        return Ok(format!("{dx},{dy},{dz}"));
    }
    let sign = if coordinate_frame { -1.0 } else { 1.0 };
    let rx = sign * get(&["8608"], &["xaxisrotation"], 'a')?;
    let ry = sign * get(&["8609"], &["yaxisrotation"], 'a')?;
    let rz = sign * get(&["8610"], &["zaxisrotation"], 'a')?;
    let ds = get(&["8611"], &["scaledifference"], 's')?;
    Ok(format!("{dx},{dy},{dz},{rx},{ry},{rz},{ds}"))
}

/// The geodetic part of a geographic CRS (or a projected CRS's base_crs):
/// returns PROJ ellipsoid/datum/prime meridian terms.
fn geodetic_terms(geog: &Value, towgs84: Option<&str>) -> Result<String, String> {
    let datum = geog
        .get("datum")
        .or_else(|| geog.get("datum_ensemble"))
        .ok_or_else(|| format!("CRS {:?} has no datum", name_of(geog)))?;
    let ellipsoid = datum
        .get("ellipsoid")
        .ok_or_else(|| format!("datum {:?} has no ellipsoid", name_of(datum)))?;
    let a = ellipsoid
        .get("semi_major_axis")
        .or_else(|| ellipsoid.get("radius"));
    let a_unit = match a {
        Some(Value::Object(o)) => unit_factor(o.get("unit"), 1.0)?,
        _ => 1.0,
    };
    let a = match a {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Object(o)) => o.get("value").and_then(|x| x.as_f64()),
        _ => None,
    }
    .ok_or_else(|| format!("ellipsoid {:?} has no semi-major axis", name_of(ellipsoid)))?
        * a_unit;
    let mut terms = format!("+a={a}");
    if let Some(rf) = ellipsoid.get("inverse_flattening").and_then(|x| x.as_f64()) {
        terms.push_str(&format!(" +rf={rf}"));
    } else if let Some(b) = ellipsoid.get("semi_minor_axis") {
        let b = match b {
            Value::Number(n) => n.as_f64(),
            Value::Object(o) => o
                .get("value")
                .and_then(|x| x.as_f64())
                .map(|v| v * unit_factor(o.get("unit"), 1.0).unwrap_or(f64::NAN)),
            _ => None,
        }
        .ok_or("ellipsoid semi-minor axis is not a number")?;
        terms.push_str(&format!(" +b={b}"));
    } else if ellipsoid.get("radius").is_some() {
        terms.push_str(&format!(" +b={a}"));
    } else {
        return Err(format!(
            "ellipsoid {:?} has no flattening",
            name_of(ellipsoid)
        ));
    }
    if let Some(pm) = datum
        .get("prime_meridian")
        .or_else(|| geog.get("prime_meridian"))
    {
        let lon = match pm.get("longitude") {
            None => 0.0,
            Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
            Some(Value::Object(o)) => {
                let val = o.get("value").and_then(|x| x.as_f64()).unwrap_or(f64::NAN);
                (val * unit_factor(o.get("unit"), std::f64::consts::PI / 180.0)?).to_degrees()
            }
            _ => f64::NAN,
        };
        if !lon.is_finite() {
            return Err("prime meridian longitude is not a number".into());
        }
        if lon != 0.0 {
            terms.push_str(&format!(" +pm={lon}"));
        }
    }
    match towgs84 {
        Some(t) => terms.push_str(&format!(" +towgs84={t}")),
        None => {
            let dname = normalize(&name_of(datum));
            let compatible = WGS84_COMPATIBLE.iter().any(|n| match n.strip_suffix('*') {
                Some(prefix) => dname.starts_with(prefix),
                None => dname == *n,
            }) || matches!(id_of(datum), Some((a, c)) if a == "EPSG" && (c == "6326" || c == "6269"));
            if !compatible {
                return Err(format!(
                    "datum {:?} needs a datum shift to WGS 84 that freestiler cannot apply exactly; \
                     reproject to EPSG:4326 before tiling, or pass a BoundCRS / PROJ string with +towgs84",
                    name_of(datum)
                ));
            }
            terms.push_str(" +towgs84=0,0,0");
        }
    }
    Ok(terms)
}

fn crs_to_proj(v: &Value, towgs84: Option<String>) -> Result<String, String> {
    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "GeographicCRS" | "GeodeticCRS" => {
            check_axes(v, true)?;
            let terms = geodetic_terms(v, towgs84.as_deref())?;
            if terms.ends_with("+towgs84=0,0,0")
                && is_wgs84_ellipsoid(&terms)
                && !terms.contains("+pm=")
            {
                return Ok(LONLAT.to_string());
            }
            Ok(format!("+proj=longlat {terms} +no_defs"))
        }
        "ProjectedCRS" => {
            let base = v.get("base_crs").ok_or("ProjectedCRS without base_crs")?;
            let conversion = v
                .get("conversion")
                .ok_or("ProjectedCRS without conversion")?;
            let method = conversion
                .get("method")
                .ok_or("conversion without method")?;
            let code: u32 = id_of(method)
                .and_then(|(a, c)| if a == "EPSG" { c.parse().ok() } else { None })
                .or_else(|| {
                    let n = normalize(&name_of(method));
                    METHODS.iter().find(|m| normalize(m.1) == n).map(|m| m.0)
                })
                .ok_or_else(|| unsupported_method(&name_of(method)))?;
            let (_, _, proj) = METHODS
                .iter()
                .find(|m| m.0 == code)
                .ok_or_else(|| unsupported_method(&name_of(method)))?;
            if code == 1024 {
                return Ok(WEB_MERCATOR.to_string());
            }
            let to_meter = check_axes(v, false)?;
            let params = conversion
                .get("parameters")
                .and_then(|p| p.as_array())
                .ok_or("conversion without parameters")?;
            let p = |code: &str| -> Result<Option<f64>, String> { param(params, code) };
            let deg = |code: &str| -> Result<f64, String> {
                p(code)?.ok_or_else(|| format!("projection parameter EPSG:{code} missing"))
            };
            let opt0 = |code: &str| -> Result<f64, String> { Ok(p(code)?.unwrap_or(0.0)) };
            let mut s = format!("+proj={proj}");
            match code {
                9807 | 9809 => {
                    s += &format!(
                        " +lat_0={} +lon_0={} +k={} +x_0={} +y_0={}",
                        deg("8801")?,
                        deg("8802")?,
                        p("8805")?.unwrap_or(1.0),
                        opt0("8806")?,
                        opt0("8807")?
                    );
                }
                9801 => {
                    let lat0 = deg("8801")?;
                    s += &format!(
                        " +lat_1={lat0} +lat_0={lat0} +lon_0={} +k={} +x_0={} +y_0={}",
                        deg("8802")?,
                        p("8805")?.unwrap_or(1.0),
                        opt0("8806")?,
                        opt0("8807")?
                    );
                }
                9802 | 9822 => {
                    s += &format!(
                        " +lat_0={} +lon_0={} +lat_1={} +lat_2={} +x_0={} +y_0={}",
                        deg("8821")?,
                        deg("8822")?,
                        deg("8823")?,
                        deg("8824")?,
                        opt0("8826")?,
                        opt0("8827")?
                    );
                }
                9804 => {
                    if opt0("8801")? != 0.0 {
                        return Err(
                            "Mercator (variant A) with a non-zero latitude of origin".into()
                        );
                    }
                    s += &format!(
                        " +lon_0={} +k={} +x_0={} +y_0={}",
                        deg("8802")?,
                        p("8805")?.unwrap_or(1.0),
                        opt0("8806")?,
                        opt0("8807")?
                    );
                }
                9805 => {
                    s += &format!(
                        " +lat_ts={} +lon_0={} +x_0={} +y_0={}",
                        deg("8823")?,
                        deg("8802")?,
                        opt0("8806")?,
                        opt0("8807")?
                    );
                }
                9820 => {
                    s += &format!(
                        " +lat_0={} +lon_0={} +x_0={} +y_0={}",
                        deg("8801")?,
                        deg("8802")?,
                        opt0("8806")?,
                        opt0("8807")?
                    );
                }
                _ => return Err(unsupported_method(&name_of(method))),
            }
            let terms = geodetic_terms(base, towgs84.as_deref())?;
            s += &format!(" {terms} +to_meter={to_meter} +no_defs");
            Ok(s)
        }
        "" => Err("CRS PROJJSON has no type".into()),
        other => Err(format!(
            "CRS type {other} ({:?}) is not supported; reproject to EPSG:4326 before tiling",
            name_of(v)
        )),
    }
}

fn is_wgs84_ellipsoid(terms: &str) -> bool {
    terms.starts_with("+a=6378137 +rf=298.257223563")
}

fn unsupported_method(name: &str) -> String {
    let names: Vec<&str> = METHODS.iter().map(|m| m.1).collect();
    format!(
        "projection method {name:?} is not supported (supported: {}); reproject to EPSG:4326 \
         before tiling",
        names.join(", ")
    )
}

/// A projection parameter by EPSG code, in degrees (angles) or metres
/// (lengths); scale factors are returned unitless.
fn param(params: &[Value], code: &str) -> Result<Option<f64>, String> {
    for p in params {
        let pc = id_of(p).map(|(_, c)| c);
        if pc.as_deref() != Some(code) {
            continue;
        }
        let value = p
            .get("value")
            .and_then(|x| x.as_f64())
            .ok_or_else(|| format!("parameter EPSG:{code} has no numeric value"))?;
        let unit = p.get("unit");
        let kind = match unit {
            Some(Value::String(s)) if s == "degree" || s == "radian" => 'a',
            Some(Value::String(s)) if s == "metre" || s == "meter" => 'l',
            Some(Value::String(s)) if s == "unity" => 's',
            Some(Value::Object(o)) => match o.get("type").and_then(|t| t.as_str()) {
                Some("AngularUnit") => 'a',
                Some("LinearUnit") => 'l',
                Some("ScaleUnit") => 's',
                _ => return Err(format!("parameter EPSG:{code} has an unknown unit type")),
            },
            None => match code {
                "8805" => 's',
                "8806" | "8807" | "8826" | "8827" => 'l',
                _ => 'a',
            },
            _ => return Err(format!("parameter EPSG:{code} has an unknown unit")),
        };
        return Ok(Some(match kind {
            'a' => (value * unit_factor(unit, std::f64::consts::PI / 180.0)?).to_degrees(),
            'l' => value * unit_factor(unit, 1.0)?,
            _ => value * unit_factor(unit, 1.0)?,
        }));
    }
    Ok(None)
}

/// Check the coordinate system is east/north (GeoParquet stores x = easting
/// or longitude regardless of the declared axis order, but a west- or
/// south-oriented system would still flip the data) and return the linear
/// unit's metres per unit for projected CRSs.
fn check_axes(v: &Value, geographic: bool) -> Result<f64, String> {
    let Some(cs) = v.get("coordinate_system") else {
        return Ok(1.0);
    };
    let axes = cs
        .get("axis")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let mut factor = None;
    for axis in &axes {
        let dir = axis.get("direction").and_then(|d| d.as_str()).unwrap_or("");
        match dir {
            "east" | "north" => {}
            "up" => continue,
            other => {
                return Err(format!(
                    "CRS {:?} has a {other:?} axis; only east/north systems are supported",
                    name_of(v)
                ))
            }
        }
        if !geographic {
            let f = unit_factor(axis.get("unit").or_else(|| cs.get("unit")), 1.0)?;
            match factor {
                None => factor = Some(f),
                Some(g) if (g - f).abs() > 1e-15 => {
                    return Err("projected CRS axes use different units".into())
                }
                _ => {}
            }
        }
    }
    Ok(factor.unwrap_or(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{Coord, LineString, Point, Polygon};

    // NAD83 / Pennsylvania North (ftUS), EPSG:2271, as written by pyproj.
    const PA_NORTH: &str = r#"{"type":"ProjectedCRS","name":"NAD83 / Pennsylvania North (ftUS)","base_crs":{"name":"NAD83","datum":{"type":"GeodeticReferenceFrame","name":"North American Datum 1983","ellipsoid":{"name":"GRS 1980","semi_major_axis":6378137,"inverse_flattening":298.257222101}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"},{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"}]},"id":{"authority":"EPSG","code":4269}},"conversion":{"name":"SPCS83 Pennsylvania North zone (US survey foot)","method":{"name":"Lambert Conic Conformal (2SP)","id":{"authority":"EPSG","code":9802}},"parameters":[{"name":"Latitude of false origin","value":40.1666666666667,"unit":"degree","id":{"authority":"EPSG","code":8821}},{"name":"Longitude of false origin","value":-77.75,"unit":"degree","id":{"authority":"EPSG","code":8822}},{"name":"Latitude of 1st standard parallel","value":41.95,"unit":"degree","id":{"authority":"EPSG","code":8823}},{"name":"Latitude of 2nd standard parallel","value":40.8833333333333,"unit":"degree","id":{"authority":"EPSG","code":8824}},{"name":"Easting at false origin","value":1968500,"unit":{"type":"LinearUnit","name":"US survey foot","conversion_factor":0.304800609601219},"id":{"authority":"EPSG","code":8826}},{"name":"Northing at false origin","value":0,"unit":{"type":"LinearUnit","name":"US survey foot","conversion_factor":0.304800609601219},"id":{"authority":"EPSG","code":8827}}]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"X","direction":"east","unit":{"type":"LinearUnit","name":"US survey foot","conversion_factor":0.304800609601219}},{"name":"Northing","abbreviation":"Y","direction":"north","unit":{"type":"LinearUnit","name":"US survey foot","conversion_factor":0.304800609601219}}]},"id":{"authority":"EPSG","code":2271}}"#;

    fn reproject(crs: &SourceCrs, x: f64, y: f64) -> (f64, f64) {
        let mut g = Geometry::Point(Point::new(x, y));
        crs.to_lonlat(&mut g).unwrap();
        match g {
            Geometry::Point(p) => (p.x(), p.y()),
            _ => unreachable!(),
        }
    }

    #[test]
    fn pa_north_us_survey_feet_matches_pyproj() {
        let crs = resolve(PA_NORTH).unwrap();
        let s = crs.proj_string().unwrap();
        assert!(
            s.contains("+proj=lcc") && s.contains("+to_meter=0.304800609601219"),
            "{s}"
        );
        // pyproj 3.8 / PROJ 9.8: Transformer.from_crs(2271, 4326, always_xy=True)
        let (lon, lat) = reproject(&crs, 2_003_437.560527763, 323_626.1972692954);
        assert!((lon - -77.62332198510).abs() < 1e-9, "{lon}");
        assert!((lat - 41.05482932662).abs() < 1e-9, "{lat}");
    }

    #[test]
    fn wgs84_aliases_are_identity() {
        for d in ["EPSG:4326", "OGC:CRS84", "crs84"] {
            assert!(resolve(d).unwrap().is_identity());
        }
        let projjson = r#"{"type":"GeographicCRS","name":"WGS 84","datum":{"type":"GeodeticReferenceFrame","name":"World Geodetic System 1984","ellipsoid":{"name":"WGS 84","semi_major_axis":6378137,"inverse_flattening":298.257223563}},"coordinate_system":{"subtype":"ellipsoidal","axis":[{"name":"Geodetic longitude","abbreviation":"Lon","direction":"east","unit":"degree"},{"name":"Geodetic latitude","abbreviation":"Lat","direction":"north","unit":"degree"}]}}"#;
        assert!(resolve(projjson).unwrap().is_identity());
    }

    #[test]
    fn web_mercator_inverse() {
        let crs = resolve("EPSG:3857").unwrap();
        let (lon, lat) = reproject(&crs, 20_037_508.342789244, 0.0);
        assert!((lon - 180.0).abs() < 1e-9 && lat.abs() < 1e-12);
    }

    #[test]
    fn refuses_unknown_datum_and_method() {
        let nad27 = PA_NORTH
            .replace("North American Datum 1983", "North American Datum 1927")
            .replace("\"code\":4269", "\"code\":4267")
            .replace("\"code\":2271", "\"code\":32028");
        let err = resolve(&nad27).unwrap_err();
        assert!(err.contains("North American Datum 1927"), "{err}");
        let omerc = PA_NORTH
            .replace(
                "Lambert Conic Conformal (2SP)",
                "Hotine Oblique Mercator (variant B)",
            )
            .replace("\"code\":9802", "\"code\":9815");
        let err = resolve(&omerc).unwrap_err();
        assert!(err.contains("Hotine"), "{err}");
        assert!(resolve("EPSG:2271").is_err());
        // PROJ shifts NAD83(HARN) by about a metre: refused, not treated as WGS 84.
        let harn = PA_NORTH.replace(
            "North American Datum 1983",
            "NAD83 (High Accuracy Reference Network)",
        );
        assert!(resolve(&harn).unwrap_err().contains("High Accuracy"));
    }

    #[test]
    fn polygon_rings_are_reprojected() {
        let crs = resolve(PA_NORTH).unwrap();
        let ring = LineString(vec![
            Coord {
                x: 2_003_000.0,
                y: 323_000.0,
            },
            Coord {
                x: 2_004_000.0,
                y: 323_000.0,
            },
            Coord {
                x: 2_004_000.0,
                y: 324_000.0,
            },
            Coord {
                x: 2_003_000.0,
                y: 323_000.0,
            },
        ]);
        let mut g = Geometry::Polygon(Polygon::new(ring.clone(), vec![ring]));
        crs.to_lonlat(&mut g).unwrap();
        let Geometry::Polygon(p) = g else {
            unreachable!()
        };
        for c in p.exterior().0.iter().chain(p.interiors()[0].0.iter()) {
            assert!(
                c.x < -77.0 && c.x > -78.0 && c.y > 40.9 && c.y < 41.1,
                "{c:?}"
            );
        }
    }
}
