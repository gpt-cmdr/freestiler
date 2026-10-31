"""Type stubs for the freestiler Rust extension module."""

def _freestile(
    layers: list[dict],
    output_path: str,
    tile_format: str,
    min_zoom: int,
    max_zoom: int,
    base_zoom: int,
    do_simplify: bool,
    generate_ids: bool,
    quiet: bool,
    drop_rate: float,
    cluster_distance: float,
    cluster_maxzoom: int,
    do_coalesce: bool,
    gzip_level: int | None = None,
    buffer: float | None = None,
) -> str: ...

def _freestile_file(
    input_path: str,
    output_path: str,
    layer_name: str,
    tile_format: str,
    min_zoom: int,
    max_zoom: int,
    base_zoom: int,
    do_simplify: bool,
    quiet: bool,
    drop_rate: float,
    cluster_distance: float,
    cluster_maxzoom: int,
    do_coalesce: bool,
) -> str: ...

def _freestile_duckdb(
    input_path: str,
    output_path: str,
    layer_name: str,
    tile_format: str,
    min_zoom: int,
    max_zoom: int,
    base_zoom: int,
    do_simplify: bool,
    quiet: bool,
    drop_rate: float,
    cluster_distance: float,
    cluster_maxzoom: int,
    do_coalesce: bool,
) -> str: ...

def _freestile_duckdb_query(
    sql: str,
    db_path: str | None,
    output_path: str,
    layer_name: str,
    tile_format: str,
    min_zoom: int,
    max_zoom: int,
    base_zoom: int,
    do_simplify: bool,
    quiet: bool,
    drop_rate: float,
    cluster_distance: float,
    cluster_maxzoom: int,
    do_coalesce: bool,
    streaming_mode: str,
) -> str: ...


class _LayeredTiler:
    def __init__(
        self,
        tile_format: str,
        min_zoom: int,
        max_zoom: int,
        base_zoom: int,
        do_simplify: bool,
        generate_ids: bool,
        quiet: bool,
        drop_rate: float,
        cluster_distance: float,
        cluster_maxzoom: int,
        do_coalesce: bool,
        gzip_level: int | None = None,
        buffer: float | None = None,
    ) -> None: ...
    def add_layer(self, layer: dict) -> int: ...
    def add_arrow_layer(
        self,
        name: str,
        source: object,
        min_zoom: int,
        max_zoom: int,
        geometry_column: str | None = None,
        crs: str | None = None,
    ) -> dict: ...
    def finish(self, output_path: str) -> str: ...
