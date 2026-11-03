"""Keeping lines that collapse to one tile pixel (FreestileLayer.keep_short_lines)."""

import inspect

import geopandas as gpd
import pytest
from shapely.geometry import LineString, MultiLineString, Polygon, box

mapbox_vector_tile = pytest.importorskip("mapbox_vector_tile")
reader_module = pytest.importorskip("pmtiles.reader")

import gzip

from freestiler import FreestileLayer, freestile, freestile_layer, freestile_layers


def _frame():
    x, y = -97.7, 30.3
    return gpd.GeoDataFrame(
        {"fid": list(range(7))},
        geometry=[
            LineString([(x, y), (x + 1e-7, y)]),
            LineString([(x + 0.01, y), (x + 0.01 + 1e-5, y + 1e-5)]),
            LineString([(x + 0.02, y), (x + 0.02, y + 3e-4)]),
            LineString([(x + 0.03, y), (x + 0.03, y)]),
            MultiLineString([[(x + 0.04, y), (x + 0.04 + 1e-7, y)],
                             [(x + 0.04, y + 1e-6), (x + 0.04 + 1e-7, y + 1e-6)]]),
            LineString([(-1e-7, 0.0), (1e-7, 0.0)]),
            LineString([(x, y), (x + 3.0, y + 2.0)]),
        ],
        crs="EPSG:4326",
    )


def _options():
    return dict(tile_format="mvt", min_zoom=0, max_zoom=12, quiet=True, buffer=5,
                gzip_level=6, generate_ids=False)


def _decode(path):
    """{zoom: {fid: geometry type}} and the raw (geometry, type) of every feature."""
    out = {}
    with open(path, "rb") as f:
        def get_bytes(offset, length):
            f.seek(offset)
            return f.read(length)

        for (z, x, y), data in reader_module.all_tiles(get_bytes):
            tile = mapbox_vector_tile.decode(gzip.decompress(data))
            for layer in tile.values():
                for feature in layer["features"]:
                    out.setdefault(z, {}).setdefault(feature["properties"]["fid"], []).append(
                        ((x, y), feature["geometry"])
                    )
    return out


def test_capability_is_detectable():
    assert "keep_short_lines" in inspect.signature(FreestileLayer).parameters
    assert "keep_short_lines" in inspect.signature(freestile_layer).parameters


def test_off_is_the_default_and_byte_identical(tmp_path):
    plain, off = tmp_path / "plain.pmtiles", tmp_path / "off.pmtiles"
    freestile(_frame(), plain, layer_name="l", **_options())
    freestile_layers([("l", FreestileLayer(input=_frame(), keep_short_lines=False))], off,
                     **_options())
    assert plain.read_bytes() == off.read_bytes()


def test_every_line_at_every_zoom(tmp_path):
    off, on = tmp_path / "off.pmtiles", tmp_path / "on.pmtiles"
    frame = _frame()
    freestile_layers([("l", freestile_layer(frame))], off, **_options())
    freestile_layers([("l", freestile_layer(frame, keep_short_lines=True))], on, **_options())
    before, after = _decode(off), _decode(on)
    fids = set(frame["fid"])
    assert any(set(before[z]) != fids for z in range(0, 13)), "the test must show the defect"
    for z in range(0, 13):
        assert set(after[z]) == fids, f"z{z}"
    # What was written stays; only absent features are added, once per zoom.
    for z, by_fid in before.items():
        for fid, pieces in by_fid.items():
            assert after[z][fid] == pieces
    for z, by_fid in after.items():
        for fid, pieces in by_fid.items():
            if fid not in before.get(z, {}):
                assert len(pieces) == 1, (z, fid, pieces)


def test_stand_in_decodes_as_a_valid_two_point_line(tmp_path):
    on = tmp_path / "on.pmtiles"
    freestile_layers([("l", freestile_layer(_frame(), keep_short_lines=True))], on, **_options())
    decoded = _decode(on)
    for z, by_fid in decoded.items():
        for fid, pieces in by_fid.items():
            for _, geometry in pieces:
                assert geometry["type"] in ("LineString", "MultiLineString")
                if geometry["type"] == "LineString":
                    assert len(geometry["coordinates"]) >= 2
                    assert geometry["coordinates"][0] != geometry["coordinates"][1]
    # Line 0 is under a pixel at z0-z12: one unit, in one tile.
    for z in range(0, 13):
        [(_, geometry)] = decoded[z][0]
        a, b = geometry["coordinates"]
        assert abs(a[0] - b[0]) + abs(a[1] - b[1]) == 1


def test_all_entry_points_agree(tmp_path):
    pa = pytest.importorskip("pyarrow")
    frame = _frame()
    a, b, c = (tmp_path / f"{n}.pmtiles" for n in "abc")
    freestile(freestile_layer(frame, keep_short_lines=True), a, layer_name="l", **_options())
    freestile_layers([("l", freestile_layer(frame, keep_short_lines=True))], b, **_options())
    table = pa.table({"fid": frame["fid"].tolist(),
                      "geometry": pa.array(frame.geometry.to_wkb().tolist(), pa.binary())})
    freestile_layers(
        [("l", FreestileLayer(input=table, crs="EPSG:4326", geometry_column="geometry",
                              keep_short_lines=True))],
        c, **_options(),
    )
    assert a.read_bytes() == b.read_bytes()
    assert _decode(a) == _decode(c)


def test_arrow_binding_defaults_to_off(tmp_path):
    pa = pytest.importorskip("pyarrow")
    frame = _frame()
    table = pa.table({"fid": frame["fid"].tolist(),
                      "geometry": pa.array(frame.geometry.to_wkb().tolist(), pa.binary())})
    a, b = tmp_path / "a.pmtiles", tmp_path / "b.pmtiles"
    freestile_layers([("l", FreestileLayer(input=table, crs="EPSG:4326"))], a, **_options())
    freestile_layers([("l", FreestileLayer(input=table, crs="EPSG:4326", keep_short_lines=False))],
                     b, **_options())
    assert a.read_bytes() == b.read_bytes()


@pytest.mark.parametrize("bad", [None, 1, "yes"])
def test_refuses_anything_but_a_bool(tmp_path, bad):
    with pytest.raises(ValueError, match="keep_short_lines"):
        freestile_layers([("l", freestile_layer(_frame(), keep_short_lines=bad))],
                         tmp_path / "x.pmtiles", **_options())


def test_refused_with_categorical_clustering(tmp_path):
    frame = gpd.GeoDataFrame({"c": ["a", "b"]}, geometry=[box(0, 0, 1, 1), box(2, 2, 3, 3)], crs="EPSG:4326")
    with pytest.raises(ValueError):
        freestile(
            {"x": freestile_layer(frame, keep_short_lines=True)},
            tmp_path / "cat.pmtiles",
            category="c",
            category_values=["a", "b"],
            cluster_distance=50,
            quiet=True,
        )


def test_polygons_keep_per_zoom_simplification_and_are_unchanged(tmp_path):
    ring = [(-97.7 + 0.02 * i / 40, 30.3 + 0.001 * ((-1) ** i)) for i in range(40)]
    frame = gpd.GeoDataFrame(
        {"fid": [100, 101]},
        geometry=[Polygon(ring + [(-97.7, 30.4)]), Polygon([(-97.0, 30.0), (-97.0 + 1e-7, 30.0),
                                                            (-97.0, 30.0 + 1e-7)])],
        crs="EPSG:4326",
    )
    a, b = tmp_path / "a.pmtiles", tmp_path / "b.pmtiles"
    for path, keep in ((a, False), (b, True)):
        freestile_layers([("l", freestile_layer(frame, polygon_simplify=0.5,
                                                keep_short_lines=keep))],
                         path, **_options())
    assert a.read_bytes() == b.read_bytes()
    decoded = _decode(b)
    for z in range(0, 13):
        assert set(decoded[z]) == {100, 101}, f"z{z}"
