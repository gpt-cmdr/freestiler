# freestiler 0.3.0.9000

* Python input can now be any Arrow stream (`__arrow_c_stream__`: a pyarrow
  Table or RecordBatchReader, `GeoDataFrame.to_arrow()`), passed to
  `freestile()`, `freestile_layers()` or `freestile_layer()`. The geometry
  column is WKB or native GeoArrow; it is read through the Arrow C stream
  interface and reprojected to EPSG:4326 in pure Rust (proj4rs, no PROJ or
  pyproj), so no GeoPandas, shapely or JSON step is needed. The CRS comes
  from the GeoArrow extension or GeoParquet `geo` metadata, or
  `freestile_layer(crs=)`; input with no CRS is refused. PROJJSON is
  converted only for Transverse Mercator, Lambert Conic Conformal (1SP/2SP),
  Albers, Mercator, Oblique Stereographic and Lambert Azimuthal Equal Area,
  on datums whose default PROJ operation to WGS 84 is null (WGS 84, NAD83,
  NAD83(NSRS2007), NAD83(2011), ETRS89, GDA94, GDA2020, NZGD2000, SIRGAS
  2000) or with an explicit Helmert BoundCRS; everything else is refused,
  never guessed. Against pyproj 3.8 / PROJ 9.8 the largest difference over
  16 US state plane, UTM, Albers and Web Mercator CRSs is 73 nm. On HEC-RAS
  meshes the archive is byte-identical to the GeoDataFrame path.

* New `buffer` option (Python `freestile(buffer=)`, Rust `TileConfig::buffer`):
  the clip buffer in pixels of a 256-pixel tile, like tippecanoe `--buffer`.
  The default stays 5% (12.8 px). Every feature is still kept at every zoom
  and vertices inside the tile do not move; a smaller buffer only repeats
  fewer features in neighbouring tiles. With `buffer=5` (tippecanoe's
  default), gzip level 6 and no IDs, an 87,039-cell HEC-RAS mesh (z10-15)
  is 9.23 MB against tippecanoe's 9.10 MB (was 10.21 MB).

* MVT doubles that round-trip exactly through f32 (for example values read
  from float32 columns) are written as MVT floats: 5 bytes instead of 9, with
  identical decoded values (-2.6% on a HEC-RAS water-surface tileset).

* A short line that Visvalingam-Whyatt pre-simplification reduced to two end
  points on one pixel was dropped at that zoom. The original line is now
  used instead, so it stays visible (a nearly closed HEC-RAS breakline
  vanished at z8).

* New `gzip_level` option (Python `freestile(gzip_level=)`, Rust
  `TileConfig::gzip_level`) sets the gzip level of tile data in the in-memory
  pipeline. The default stays the fast level 1; tile contents never change. On
  an 87,039-cell HEC-RAS mesh (z10-15) level 6 makes the archive 14% smaller
  (13.8 to 11.9 MB) for about 10% more tiling time.

* Multi-layer tilesets no longer hold every layer in memory. A new Rust
  `LayeredTiler` tiles one layer at a time, spools its per-tile encodings to a
  temporary file, and assembles each tile from its layers at the end. In both
  MVT and MLT a tile is a sequence of independently encoded layers, so the
  archive is byte-identical to the all-layer path. Python's `freestile()` uses
  it for every multi-layer call, and the new `freestile_layers()` accepts
  `(name, source)` pairs or a generator, where a source may be a GeoDataFrame,
  a `FreestileLayer`, a GeoParquet path or a callable, so only one layer is
  loaded at a time on the Python side too.

* Feature coalescing (`coalesce = TRUE`) now groups features in order of first
  appearance instead of hash-map order, so its output is reproducible from run
  to run.

* `freestile()` now accepts `category`, `category_values`, and
  `cluster_min_points` for a single POINT sf object or GeoDataFrame. Category
  counts use the same Supercluster engine as file clustering, with individual
  points above `cluster_maxzoom` in the same archive. The in-memory path works
  without GeoParquet or DuckDB support, including in the CRAN build. Existing
  calls without category options retain their clustering behavior.

# freestiler 0.3.0

* MVT output with `simplification = FALSE` now preserves line positions when
  a sub-pixel line part disappears during encoding. This affects multipart
  lines and single lines split by tile clipping. Default simplification and
  MLT output are unaffected.

* Requests for native features omitted from the CRAN build now explain how to
  install from R-Universe, including the platform limits of Rust DuckDB and
  streaming and the R DuckDB fallback.
* Native GeoParquet and DuckDB readers preserve DATE and timestamp properties
  as ISO 8601 text instead of silently replacing them with nulls (#18).
  Timestamps use a `T` separator; zoned timestamps are normalized to UTC
  with a `Z` suffix, independent of the DuckDB session time zone. Genuine
  nulls remain null. The R DuckDB fallback uses the same timestamp format;
  direct `freestile()` calls on sf objects retain their existing behavior.
  Formatting zoned query timestamps does not require DuckDB's ICU extension.
* Rust DuckDB queries now export geometry only as WKB, fixing failures with
  CRS-tagged geometry such as `ST_Read()` shapefiles (#19). CRS-tagged query
  results are now automatically reprojected to WGS84, including streaming
  queries. Reprojection preserves longitude/latitude axis order in both the
  native reader and the R DuckDB fallback used by CRAN builds.
* Experimental ordered categorical clustering is available through
  `freestile_file()` in R and Python (`category`, `category_values`,
  `cluster_min_points`). It uses the pinned Supercluster 8.0.1/KDBush 4.1.0
  port, with 512-tile pixel radius, typed category counts and expansion zoom.
  This increment requires a POINT GeoParquet file, `cluster_maxzoom = max_zoom`,
  and no thinning/coalescing; use a separate dot source above the cutoff.
  The global index is compact but resident, not out-of-core. Existing
  noncategorical clustering is unchanged pending the broader API migration.
  Output is spooled one tile at a time, exact singleton originals live on disk,
  and `FREESTILER_CLUSTER_TILE_BUDGET_MB` (default 256) guards decoded tile buffers
  and the tile-coordinate index. Numeric/string-union abbreviation fields are
  omitted from TileJSON's single-type field map.
* `freestile_h3()` now supports aggregate-only archives with
  `include_points = FALSE`, covering the complete requested zoom range without
  fetching a raw-point frame. Optional `category` and `category_values` add
  `point_count`, `<category>:<value>` counts, an `_other` count for missing or
  unlisted values, and modal category/count/share/tie fields. Ties have no
  selected category. R and Python share these semantics. All finalized hex
  layers are still held in memory; bounded polygon output is separate work.
* H3 integer aggregates are checked before host conversion and encoded as
  integers, including values above R's 32-bit integer range (supported exact
  range: +/- (2^53 - 1)). Antimeridian repair now touches only crossing cells.
* PMTiles field metadata now uses TileJSON `Number`, `String`, and `Boolean`
  types, with a `field_order` extension preserving requested property order.
  `view_h3_tiles()` defaults to `point_count` for categorical output and skips
  H3 bookkeeping fields when choosing a numeric aggregate.

* The streaming point pipeline (`freestile_query()`) now partitions the query
  result on disk and tiles each partition independently instead of sorting
  the whole dataset at once. This removes the global sort that previously
  spilled tens of gigabytes on very large inputs and makes billion-point
  tiling feasible with bounded memory. Runs that would exhaust a resource now
  stop with a clear error (naming the tile or region and suggesting
  `drop_rate` or a higher `min_zoom`) rather than degrading, and a failed run
  never disturbs an existing output. Optional environment variables:
  `FREESTILER_TEMP_DIR` (where bulk temporary data goes),
  `FREESTILER_DUCKDB_MEMORY`, and `FREESTILER_STREAM_WORKERS`.
* With `drop_rate`, streaming point thinning is now computed per partition:
  the per-zoom density is unchanged, but the exact set of retained points can
  differ from earlier releases (the sampling phase restarts per partition, so
  membership shifts are not limited to partition boundaries). Without
  `drop_rate` output is unchanged.

* Tile generation now uses dramatically less memory on large inputs. Encoded
  tiles are compressed and spooled to a temporary file as they are produced
  instead of being accumulated in RAM across all zoom levels, and the PMTiles
  archive is assembled by streaming from that spool. Together with allocation
  fixes in the GeoParquet reader and MVT encoder, peak memory for
  multi-million-feature polygon inputs drops to roughly the cost of the
  decoded features themselves. Note the tradeoff: tile data is staged on disk,
  so a build transiently needs about twice the final archive size in free
  space.
* Output archives are now written atomically: tiles are assembled in a
  temporary sibling file that is renamed over the destination only on
  success. A failed run no longer leaves a partial archive, and an existing
  output file is preserved until its replacement is complete. Relatedly,
  `overwrite = TRUE` is now respected through the DuckDB and CRS-reprojection
  fallback paths instead of being reset to `FALSE` internally.
* Identical tiles are no longer deduplicated within the archive (a
  micro-optimization measured at ~0.01% of archive size on real data); the
  PMTiles `clustered` header flag is now computed from the actual tile data
  layout rather than always claimed.
* macOS builds now use Rust's baseline deployment target for Rust and dependency
  C objects instead of the host OS version. This prevents warnings when R links
  for an older macOS version. Explicit user deployment targets are preserved.

# freestiler 0.2.0

* Thin (sub-pixel-width) polygons no longer flicker in and out across zoom
  levels (#13). A polygon that collapses on a tile's integer pixel grid is
  now replaced by a one-pixel square at its centroid instead of being
  dropped, so narrow features stay continuously visible. Ring winding is
  also normalized to the MVT specification (exterior rings positive area,
  interior rings negative), zero-area degenerate rings are dropped rather
  than emitted as invalid polygons, and quantization-induced spikes
  (out-and-back needle vertices) are removed.
* `freestile_h3()` is a new function for dynamic hexagonal binning. It
  aggregates points into H3 hexagons at zoom-appropriate resolutions via
  DuckDB's H3 community extension and writes a multi-layer `.pmtiles`
  archive where low zooms show coarse hexes, intermediate zooms show
  progressively finer hexes, and zooms at or above `base_zoom` show the raw
  points. Aggregation rules are user-defined SQL expressions
  (e.g. `c(n = "COUNT(*)", avg_pop = "AVG(pop)")`). Opt-in `fade = TRUE`
  produces overlapping zoom windows so adjacent hex resolutions can
  cross-fade visually.
* `view_h3_tiles()` is a companion viewer that auto-styles a `freestile_h3()`
  archive in `mapgl`, detecting clean-break vs cross-fade mode from the
  PMTiles metadata.
* Hexagons that cross the antimeridian are split at +/-180 degrees rather
  than rendering as world-spanning slivers.
* See `vignette("h3-hexagonal-binning")` for a walkthrough.

# freestiler 0.1.7

* Updated the CRAN Rust build path to use a dependency graph compatible with
  rustc/cargo 1.77.2.

# freestiler 0.1.0

Initial release.

## Tile generation

* `freestile()` creates PMTiles archives from sf data frames with zero external
  dependencies (no tippecanoe, no Java, no Go).
* Supports **MapLibre Tiles (MLT)** and **Mapbox Vector Tiles (MVT)** output
  formats.
* Multi-layer output via named lists or `freestile_layer()` per-layer zoom
  control.

## Geometry types

* POINT, MULTIPOINT, LINESTRING, MULTILINESTRING, POLYGON, MULTIPOLYGON.
* Automatic CRS transformation to WGS84.
* Z/M dimension handling (dropped automatically).

## Performance features

* Parallel tile encoding with rayon (across tiles and within tiles).
* Tile-pixel grid snapping for zoom-adaptive simplification without slivers.
* Buffered tile assignment and clipping for seamless tile boundaries.

## Feature management

* `drop_rate` exponential feature thinning with Morton-curve spatial ordering
  for points and area-based ordering for polygons/lines.
* `base_zoom` control for ensuring all features present at higher zooms.
* `cluster_distance` point clustering with `point_count` attribute.
* `coalesce` line merging and polygon grouping.

## MLT encoder

* Spec-compliant MapLibre Tile encoder with varint, delta, RLE, and dictionary
  encoding.
* Validated against mlt-core 0.1.2 reference decoder.
