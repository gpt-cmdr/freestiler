"""Arrow / GeoArrow input reprojected in Rust, and the tile buffer option."""

import gzip
import json

import geopandas as gpd
import numpy as np
import pytest
from shapely.geometry import box

from freestiler import freestile, freestile_layer, freestile_layers

pa = pytest.importorskip("pyarrow")
pq = pytest.importorskip("pyarrow.parquet")
pyproj = pytest.importorskip("pyproj")

try:
    from freestiler._freestiler import _reproject_to_lonlat
except ImportError:  # built without the arrow feature
    pytest.skip("freestiler built without Arrow input", allow_module_level=True)


def _tiles(path):
    from pmtiles.reader import MmapSource, all_tiles

    with open(path, "rb") as f:
        return {key: gzip.decompress(data) for key, data in all_tiles(MmapSource(f))}


def _mesh(crs="EPSG:4326"):
    cells = [box(-78 + i * 0.002, 40 + j * 0.002, -78 + (i + 1) * 0.002, 40 + (j + 1) * 0.002)
             for i in range(30) for j in range(30)]
    g = gpd.GeoDataFrame(
        {"cell_id": np.arange(len(cells), dtype="int64"),
         "depth": np.arange(len(cells)) * 0.25,
         "name": [f"c{k}" for k in range(len(cells))]},
        geometry=cells, crs="EPSG:4326")
    return g.to_crs(crs)


def _arrow(gdf, tmp_path, name="layer.parquet"):
    """A pyarrow Table read from GeoParquet keeps the `geo` CRS metadata."""
    path = tmp_path / name
    gdf.to_parquet(path, geometry_encoding="WKB", write_covering_bbox=False)
    return pq.read_table(path)


def test_wgs84_arrow_matches_geodataframe_byte_for_byte(tmp_path):
    pytest.importorskip("pmtiles")
    gdf = _mesh()
    a, b = tmp_path / "gdf.pmtiles", tmp_path / "arrow.pmtiles"
    freestile_layers({"cells": gdf}, a, min_zoom=8, max_zoom=13, quiet=True)
    freestile_layers({"cells": _arrow(gdf, tmp_path)}, b, min_zoom=8, max_zoom=13, quiet=True)
    assert a.read_bytes() == b.read_bytes()


def test_projected_arrow_is_reprojected_in_rust(tmp_path):
    pytest.importorskip("pmtiles")
    wgs, ftus = _mesh(), _mesh("EPSG:2271")
    a, b = tmp_path / "gdf.pmtiles", tmp_path / "arrow.pmtiles"
    freestile(wgs, a, layer_name="cells", min_zoom=8, max_zoom=13, quiet=True)
    freestile(_arrow(ftus, tmp_path), b, layer_name="cells", min_zoom=8, max_zoom=13, quiet=True)
    ta, tb = _tiles(a), _tiles(b)
    assert ta.keys() == tb.keys()
    # Sub-micrometre coordinate differences can move a vertex by one tile
    # unit at a rounding boundary; nearly every tile must still be identical.
    same = sum(ta[k] == tb[k] for k in ta)
    assert same >= 0.9 * len(ta)


def test_arrow_without_crs_is_refused(tmp_path):
    gdf = _mesh("EPSG:2271")
    table = pa.table({"cell_id": gdf["cell_id"].to_numpy(),
                      "geometry": pa.array(gdf.geometry.to_wkb(), pa.binary())})
    with pytest.raises(ValueError, match="no CRS"):
        freestile(table, tmp_path / "x.pmtiles", max_zoom=10, quiet=True)
    # Supplying the CRS explicitly works, as a pyproj CRS or PROJJSON text.
    out = tmp_path / "ok.pmtiles"
    freestile({"cells": freestile_layer(table, crs=pyproj.CRS.from_epsg(2271))}, out,
              max_zoom=10, quiet=True)
    assert out.stat().st_size > 0


def test_geopandas_to_arrow_extension_crs(tmp_path):
    gdf = _mesh("EPSG:3857")
    if not hasattr(gdf, "to_arrow"):
        pytest.skip("geopandas without to_arrow")
    out = tmp_path / "x.pmtiles"
    freestile(pa.table(gdf.to_arrow(geometry_encoding="geoarrow")), out, max_zoom=10, quiet=True)
    assert out.stat().st_size > 0


def test_unsupported_datum_is_refused(tmp_path):
    gdf = _mesh("EPSG:26715")  # NAD27 / UTM 15N needs a grid shift
    with pytest.raises(ValueError, match="North American Datum 1927"):
        freestile(_arrow(gdf, tmp_path), tmp_path / "x.pmtiles", max_zoom=10, quiet=True)


def test_reprojection_matches_pyproj():
    crs = pyproj.CRS.from_epsg(2271)
    lon, lat = np.meshgrid(np.linspace(-80.5, -74.7, 25), np.linspace(40.6, 42.5, 25))
    x, y = pyproj.Transformer.from_crs(4326, crs, always_xy=True).transform(lon.ravel(), lat.ravel())
    _, flon, flat = _reproject_to_lonlat(crs.to_json(), list(x), list(y))
    _, _, d = pyproj.Geod(ellps="WGS84").inv(lon.ravel(), lat.ravel(), flon, flat)
    assert np.max(d) < 0.001  # metres


def test_buffer_keeps_every_feature_and_shrinks(tmp_path):
    pytest.importorskip("pmtiles")
    gdf = _mesh()
    wide, narrow = tmp_path / "wide.pmtiles", tmp_path / "narrow.pmtiles"
    freestile(gdf, wide, layer_name="cells", min_zoom=10, max_zoom=14, quiet=True)
    freestile(gdf, narrow, layer_name="cells", min_zoom=10, max_zoom=14, quiet=True, buffer=5)
    assert narrow.stat().st_size < wide.stat().st_size
    mvt = pytest.importorskip("mapbox_vector_tile")

    for path in (wide, narrow):
        per_zoom = {}
        for (z, _, _), raw in _tiles(path).items():
            for f in mvt.decode(raw)["cells"]["features"]:
                per_zoom.setdefault(z, set()).add(f["properties"]["cell_id"])
        assert all(len(ids) == len(gdf) for ids in per_zoom.values()), path


@pytest.mark.parametrize("value", [-1, 129, float("nan"), True])
def test_buffer_rejects_out_of_range(tmp_path, value):
    with pytest.raises(ValueError, match="buffer"):
        freestile(_mesh(), tmp_path / "x.pmtiles", max_zoom=10, quiet=True, buffer=value)
