"""Opt-in compaction for sparse tilesets: deduplicate, sort_values and
FreestileLayer.line_simplify."""

import gzip
import inspect
import math

import geopandas as gpd
import pyarrow as pa
import pytest
import shapely
from shapely.geometry import LineString, box

mapbox_vector_tile = pytest.importorskip("mapbox_vector_tile")
reader_module = pytest.importorskip("pmtiles.reader")

from freestiler import FreestileLayer, freestile, freestile_layer, freestile_layers


def _extent():
    return gpd.GeoDataFrame({"name": ["model"]}, geometry=[box(-77.8, 40.9, -77.6, 41.05)],
                            crs="EPSG:4326")


def _lines():
    rows, geoms = [], []
    for k in range(30):
        y0 = 40.92 + k * 0.004
        pts = [(-77.78 + 0.15 * t / 200, y0 + 0.0008 * math.sin(t / 200 * 41 + k)
                + 0.0002 * math.cos(t / 200 * 233)) for t in range(200)]
        geoms.append(LineString(pts))
        rows.append({"fid": k, "Name": f"line {29 - k}", "spacing": 100.0 + (k % 5) * 0.5})
    for k in range(6):
        x = -77.7 + k * 0.01
        geoms.append(LineString([(x, 41.0), (x + 2e-5, 41.00001), (x + 4e-5, 41.0)]))
        rows.append({"fid": 100 + k, "Name": f"short {k}", "spacing": 1.25})
    return gpd.GeoDataFrame(rows, geometry=geoms, crs="EPSG:4326")


def _options(**extra):
    return dict(tile_format="mvt", min_zoom=4, max_zoom=14, quiet=True, buffer=5,
                gzip_level=6, generate_ids=False, **extra)


def _tiles(path):
    with open(path, "rb") as f:
        def get_bytes(offset, length):
            f.seek(offset)
            return f.read(length)

        header = reader_module.Reader(get_bytes).header()
        tiles = {k: gzip.decompress(v) for k, v in reader_module.all_tiles(get_bytes)}
    return header, tiles


def _decoded(raw):
    return mapbox_vector_tile.decode(raw)


def _fids(path):
    out = {}
    for (z, _, _), raw in _tiles(path)[1].items():
        for name, layer in _decoded(raw).items():
            for feature in layer["features"]:
                out.setdefault((name, z), set()).add(feature["properties"].get("fid"))
    return out


def _vertices(path, name):
    n = 0
    for raw in _tiles(path)[1].values():
        layer = _decoded(raw).get(name)
        for feature in (layer or {}).get("features", []):
            n += len(shapely.get_coordinates(shapely.geometry.shape(feature["geometry"])))
    return n


def test_capabilities_are_detectable():
    for fn in (freestile, freestile_layers):
        params = inspect.signature(fn).parameters
        assert params["deduplicate"].default is False
        assert params["sort_values"].default is False
    assert inspect.signature(FreestileLayer).parameters["line_simplify"].default is None
    assert "line_simplify" in inspect.signature(freestile_layer).parameters


def test_options_off_are_byte_identical(tmp_path):
    plain, off = tmp_path / "plain.pmtiles", tmp_path / "off.pmtiles"
    layers = lambda: {"extent": _extent(), "lines": _lines()}
    freestile_layers(layers(), plain, **_options())
    freestile_layers({"extent": _extent(), "lines": freestile_layer(_lines(), line_simplify=None)},
                     off, **_options(deduplicate=False, sort_values=False))
    assert plain.read_bytes() == off.read_bytes()


def test_deduplicate_and_sort_change_no_decoded_tile(tmp_path):
    plain, compact = tmp_path / "plain.pmtiles", tmp_path / "compact.pmtiles"
    freestile_layers({"extent": _extent(), "lines": _lines()}, plain, **_options())
    freestile_layers({"extent": _extent(), "lines": _lines()}, compact,
                     **_options(deduplicate=True, sort_values=True))
    ha, a = _tiles(plain)
    hb, b = _tiles(compact)
    assert a.keys() == b.keys()
    for key in a:
        assert _decoded(a[key]) == _decoded(b[key]), key
    assert hb["addressed_tiles_count"] == ha["addressed_tiles_count"]
    assert hb["tile_contents_count"] < hb["addressed_tiles_count"]
    assert hb["tile_entries_count"] < hb["addressed_tiles_count"]
    assert hb["clustered"]
    assert compact.stat().st_size < plain.stat().st_size


def test_freestile_accepts_the_archive_options(tmp_path):
    out = tmp_path / "single.pmtiles"
    freestile(_extent(), out, layer_name="extent", **_options(deduplicate=True, sort_values=True))
    header, _ = _tiles(out)
    assert header["tile_contents_count"] < header["addressed_tiles_count"]


def test_line_simplify_keeps_features_and_drops_vertices(tmp_path):
    base, dp = tmp_path / "base.pmtiles", tmp_path / "dp.pmtiles"
    freestile_layers({"lines": _lines()}, base, **_options())
    freestile_layers({"lines": freestile_layer(_lines(), line_simplify=1.0)}, dp, **_options())
    before, after = _fids(base), _fids(dp)
    for key, fids in before.items():
        assert fids <= after.get(key, set()), key
    assert _vertices(dp, "lines") < _vertices(base, "lines")


def test_line_simplify_on_arrow_input_with_keep_short_lines(tmp_path):
    frame = _lines()
    table = pa.table({"fid": frame["fid"], "Name": frame["Name"], "spacing": frame["spacing"],
                      "geometry": shapely.to_wkb(frame.geometry.values)})
    out = tmp_path / "arrow.pmtiles"
    freestile_layers([("lines", FreestileLayer(input=table, crs="EPSG:4326", line_simplify=1.0,
                                               keep_short_lines=True))], out, **_options())
    fids = _fids(out)
    for z in range(4, 15):
        assert fids[("lines", z)] == set(frame["fid"]), z


@pytest.mark.parametrize("bad", [0, -1, 64.5, float("nan"), True, "1"])
def test_bad_line_simplify_refuses(tmp_path, bad):
    with pytest.raises(ValueError, match="line_simplify"):
        freestile_layers({"lines": freestile_layer(_lines(), line_simplify=bad)},
                         tmp_path / "bad.pmtiles", **_options())


@pytest.mark.parametrize("name", ["deduplicate", "sort_values"])
def test_archive_flags_must_be_bool(tmp_path, name):
    with pytest.raises(ValueError, match=name):
        freestile_layers({"lines": _lines()}, tmp_path / "bad.pmtiles", **_options(**{name: 1}))
