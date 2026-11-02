"""Per-zoom polygon simplification (FreestileLayer.polygon_simplify)."""

import inspect
import math

import geopandas as gpd
import pytest
from shapely.geometry import Polygon, Point, box

import freestiler
from freestiler import FreestileLayer, freestile, freestile_layer, freestile_layers


def _star(cx, cy, radius, n=4000):
    coords = []
    for i in range(n):
        a = i / n * math.tau
        r = radius * (1 + 0.3 * math.sin(9 * a) + 0.002 * math.sin(211 * a))
        coords.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return coords


def _frame():
    return gpd.GeoDataFrame(
        {"name": ["big", "tiny"]},
        geometry=[
            Polygon(_star(-98.7, 29.95, 0.15), [_star(-98.7, 29.95, 0.05, 500)]),
            Polygon(_star(-98.3, 29.6, 2e-6, 12)),
        ],
        crs="EPSG:4326",
    )


def _options():
    return dict(min_zoom=0, max_zoom=12, quiet=True, buffer=5, generate_ids=True, gzip_level=6)


def test_capability_is_detectable():
    assert "polygon_simplify" in inspect.signature(FreestileLayer).parameters
    assert "polygon_simplify" in inspect.signature(freestile_layer).parameters


def test_default_is_the_regular_path(tmp_path):
    plain, wrapped = tmp_path / "plain.pmtiles", tmp_path / "wrapped.pmtiles"
    freestile(_frame(), plain, layer_name="lc", **_options())
    freestile_layers([("lc", FreestileLayer(input=_frame()))], wrapped, **_options())
    assert plain.read_bytes() == wrapped.read_bytes()


def test_opt_in_changes_output_and_all_entry_points_agree(tmp_path):
    frame = _frame()
    a, b, c = (tmp_path / f"{n}.pmtiles" for n in "abc")
    off = tmp_path / "off.pmtiles"
    freestile(frame, off, layer_name="lc", **_options())
    freestile(freestile_layer(frame, polygon_simplify=1.0), a, layer_name="lc", **_options())
    freestile_layers([("lc", freestile_layer(frame, polygon_simplify=1.0))], b, **_options())
    pa = pytest.importorskip("pyarrow")
    if not hasattr(freestiler._freestiler._LayeredTiler, "add_arrow_layer"):
        pytest.skip("wheel built without Arrow input")
    table = frame.to_arrow()
    freestile_layers(
        [("lc", FreestileLayer(input=pa.table(table), crs="EPSG:4326", polygon_simplify=1.0))],
        c,
        **_options(),
    )
    assert a.read_bytes() == b.read_bytes()
    assert a.read_bytes() != off.read_bytes()
    assert a.stat().st_size < off.stat().st_size
    assert c.stat().st_size == b.stat().st_size


def test_every_feature_at_every_zoom(tmp_path):
    pmtiles = pytest.importorskip("pmtiles.reader")
    mvt = pytest.importorskip("mapbox_vector_tile")
    path = tmp_path / "on.pmtiles"
    freestile_layers([("lc", freestile_layer(_frame(), polygon_simplify=1.0))], path, **_options())
    seen = {}
    with open(path, "rb") as fh:
        reader = pmtiles.Reader(pmtiles.MmapSource(fh))
        for (z, _x, _y), data in pmtiles.all_tiles(reader.get_bytes):
            import gzip
            decoded = mvt.decode(gzip.decompress(data))
            for feature in decoded["lc"]["features"]:
                seen.setdefault(z, set()).add(feature["properties"]["name"])
    assert sorted(seen) == list(range(13))
    assert all(names == {"big", "tiny"} for names in seen.values())


@pytest.mark.parametrize("bad", [0, -1, 65, True, "1"])
def test_bad_tolerance_is_refused(tmp_path, bad):
    with pytest.raises(ValueError, match="polygon_simplify"):
        freestile_layers(
            [("lc", freestile_layer(_frame(), polygon_simplify=bad))],
            tmp_path / "bad.pmtiles",
            **_options(),
        )


def test_points_in_an_opted_in_layer_keep_the_regular_path(tmp_path):
    frame = gpd.GeoDataFrame({"k": [1]}, geometry=[Point(-98.7, 29.95)], crs="EPSG:4326")
    off, on = tmp_path / "off.pmtiles", tmp_path / "on.pmtiles"
    freestile_layers([("p", frame)], off, **_options())
    freestile_layers([("p", freestile_layer(frame, polygon_simplify=1.0))], on, **_options())
    assert off.read_bytes() == on.read_bytes()


def test_categorical_clustering_refuses_it(tmp_path):
    frame = gpd.GeoDataFrame({"c": ["a", "b"]}, geometry=[box(0, 0, 1, 1), box(2, 2, 3, 3)], crs="EPSG:4326")
    with pytest.raises(ValueError):
        freestile(
            {"x": freestile_layer(frame, polygon_simplify=1.0)},
            tmp_path / "cat.pmtiles",
            category="c",
            category_values=["a", "b"],
            cluster_distance=50,
            quiet=True,
        )
