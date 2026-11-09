use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::hash::Hasher;
use std::io::{copy, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flate2::write::GzEncoder;
use flate2::Compression;
use pmtiles2::util::{tile_id, write_directories};
use pmtiles2::{Compression as PmCompression, Entry, Header, TileType};
use serde_json::{json, Value};

use crate::tiler::TileCoord;

const PMTILES_HEADER_BYTES: u64 = 127;

/// Tile format selection
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TileFormat {
    Mvt,
    Mlt,
}

/// Metadata for a single layer in the PMTiles archive
pub struct LayerMeta {
    pub name: String,
    pub property_names: Vec<String>,
    pub property_types: Vec<String>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub geometry_type: Option<String>,
}

/// Gzip-compress a tile at level 1 (fast)
pub fn gzip_compress(data: &[u8]) -> Result<Vec<u8>, String> {
    gzip_compress_level(data, None)
}

/// Gzip-compress at `level` (0-9), or at the fast level 1 when `None`.
pub fn gzip_compress_level(data: &[u8], level: Option<u32>) -> Result<Vec<u8>, String> {
    let level = match level {
        None => Compression::fast(),
        Some(level) if level <= 9 => Compression::new(level),
        Some(level) => return Err(format!("gzip level {} is outside 0-9", level)),
    };
    let mut encoder = GzEncoder::new(Vec::new(), level);
    encoder
        .write_all(data)
        .map_err(|e| format!("Compression error: {}", e))?;
    encoder
        .finish()
        .map_err(|e| format!("Compression finish error: {}", e))
}

/// Disk spool for compressed tiles: tile bytes go to a temp file as they are
/// produced, so only the directory entries (~32 bytes/tile) stay in memory.
/// The spool file is deleted when the spool is dropped.
pub struct TileSpool {
    path: PathBuf,
    file: BufWriter<File>,
    offset: u64,
    entries: Vec<Entry>,
    /// With deduplication on: (length, hash) of each distinct tile payload ->
    /// spool offsets holding a payload with that key. A hit is confirmed byte
    /// for byte against the spool, so a hash collision never aliases tiles.
    dedup: Option<HashMap<(u32, u64), Vec<u64>>>,
    contents: u64,
}

impl TileSpool {
    pub fn new() -> Result<Self, String> {
        Self::new_in(&std::env::temp_dir())
    }

    /// Create the spool inside `dir` (e.g. a private per-run directory).
    pub fn new_in(dir: &Path) -> Result<Self, String> {
        // Owner-only permissions: the spool may live in a shared temp
        // directory and holds the full tile data.
        let (file, path) = create_exclusive_temp(dir, "freestiler_tiles", true)?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
            offset: 0,
            entries: Vec::new(),
            dedup: None,
            contents: 0,
        })
    }

    /// Store identical tile payloads once, as `pmtiles convert` does: a
    /// repeated payload is not written again and its directory entry points at
    /// the first copy, and consecutive tile IDs sharing one payload become one
    /// run-length entry when the archive is assembled. Every tile is still
    /// addressed and decodes to the same bytes; only storage changes. Off by
    /// default, so default archives stay byte-identical.
    pub fn set_deduplicate(&mut self, on: bool) {
        self.dedup = on.then(HashMap::new);
    }

    /// Number of distinct payloads written to the spool.
    pub fn contents(&self) -> u64 {
        self.contents
    }

    /// Offset of an earlier payload byte-identical to `bytes`, if any.
    fn find_duplicate(&mut self, key: (u32, u64), bytes: &[u8]) -> Result<Option<u64>, String> {
        let candidates = match self.dedup.as_ref().and_then(|m| m.get(&key)) {
            Some(c) if !c.is_empty() => c.clone(),
            _ => return Ok(None),
        };
        self.file
            .flush()
            .map_err(|e| format!("Cannot flush tile spool: {}", e))?;
        let mut stored = vec![0u8; bytes.len()];
        let mut found = None;
        let file = self.file.get_mut();
        for offset in candidates {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| format!("Cannot seek tile spool: {}", e))?;
            file.read_exact(&mut stored)
                .map_err(|e| format!("Cannot read tile spool: {}", e))?;
            if stored == bytes {
                found = Some(offset);
                break;
            }
        }
        file.seek(SeekFrom::End(0))
            .map_err(|e| format!("Cannot seek tile spool: {}", e))?;
        Ok(found)
    }

    /// Compress a raw tile and append it to the spool.
    pub fn write_tile(&mut self, coord: TileCoord, bytes: &[u8]) -> Result<(), String> {
        let compressed = gzip_compress(bytes)?;
        self.write_compressed_tile(coord, &compressed)
    }

    /// Append an already gzip-compressed tile to the spool.
    pub fn write_compressed_tile(
        &mut self,
        coord: TileCoord,
        compressed: &[u8],
    ) -> Result<(), String> {
        let tile_id = tile_id(coord.z, coord.x as u64, coord.y as u64);
        let length = compressed.len() as u32;
        let key = if self.dedup.is_some() {
            let mut hasher = DefaultHasher::new();
            hasher.write(compressed);
            let key = (length, hasher.finish());
            if let Some(offset) = self.find_duplicate(key, compressed)? {
                self.entries.push(Entry {
                    tile_id,
                    offset,
                    length,
                    run_length: 1,
                });
                return Ok(());
            }
            Some(key)
        } else {
            None
        };

        self.file
            .write_all(compressed)
            .map_err(|e| format!("Cannot write tile spool: {}", e))?;

        if let (Some(key), Some(map)) = (key, self.dedup.as_mut()) {
            map.entry(key).or_default().push(self.offset);
        }
        self.entries.push(Entry {
            tile_id,
            offset: self.offset,
            length,
            run_length: 1,
        });
        self.offset += compressed.len() as u64;
        self.contents += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn flush(&mut self) -> Result<(), String> {
        self.file
            .flush()
            .map_err(|e| format!("Cannot flush tile spool: {}", e))
    }
}

impl Drop for TileSpool {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Append-only disk spool of raw byte ranges, read back by extent. Deleted on
/// drop. Holds per-layer tile encodings until tiles are assembled.
pub struct RawSpool {
    path: PathBuf,
    writer: BufWriter<File>,
    reader: File,
    offset: u64,
}

impl RawSpool {
    pub fn new() -> Result<Self, String> {
        let (file, path) = create_exclusive_temp(&std::env::temp_dir(), "freestiler_layers", true)?;
        let reader = file
            .try_clone()
            .map_err(|e| format!("Cannot reopen layer spool: {}", e))?;
        Ok(Self {
            path,
            writer: BufWriter::new(file),
            reader,
            offset: 0,
        })
    }

    /// Append bytes and return their `(offset, length)` extent.
    pub fn append(&mut self, bytes: &[u8]) -> Result<(u64, u32), String> {
        let length =
            u32::try_from(bytes.len()).map_err(|_| "Tile layer larger than 4 GiB".to_string())?;
        self.writer
            .write_all(bytes)
            .map_err(|e| format!("Cannot write layer spool: {}", e))?;
        let extent = (self.offset, length);
        self.offset += u64::from(length);
        Ok(extent)
    }

    /// Append the bytes of `extent` to `out`.
    pub fn read_into(&mut self, extent: (u64, u32), out: &mut Vec<u8>) -> Result<(), String> {
        use std::io::Read;
        if extent
            .0
            .checked_add(u64::from(extent.1))
            .map_or(true, |end| end > self.offset)
        {
            return Err("Layer spool extent out of range".to_string());
        }
        self.writer
            .flush()
            .map_err(|e| format!("Cannot flush layer spool: {}", e))?;
        let start = out.len();
        out.resize(start + extent.1 as usize, 0);
        self.reader
            .seek(SeekFrom::Start(extent.0))
            .and_then(|_| self.reader.read_exact(&mut out[start..]))
            .map_err(|e| format!("Cannot read layer spool: {}", e))
    }
}

impl Drop for RawSpool {
    fn drop(&mut self) {
        let _ = self.writer.flush();
        let _ = fs::remove_file(&self.path);
    }
}

/// Removes the file at `path` on drop unless disarmed.
struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Assemble a PMTiles archive from a tile spool.
///
/// The archive is first written to a unique sibling temp file in the output's
/// directory, then atomically renamed onto `output_path` — an existing
/// destination is replaced on success and left untouched on failure.
pub fn write_pmtiles_from_spool(
    output_path: &str,
    spool: &mut TileSpool,
    format: TileFormat,
    layers: &[LayerMeta],
    min_zoom: u8,
    max_zoom: u8,
    bounds: (f64, f64, f64, f64), // (west, south, east, north)
) -> Result<(), String> {
    write_pmtiles_from_spool_metadata(
        output_path,
        spool,
        format,
        layers,
        min_zoom,
        max_zoom,
        bounds,
        None,
    )
}

/// Extra build provenance lives in a namespaced key; callers cannot override
/// vector_layers or the archive header through it.
pub fn write_pmtiles_from_spool_metadata(
    output_path: &str,
    spool: &mut TileSpool,
    format: TileFormat,
    layers: &[LayerMeta],
    min_zoom: u8,
    max_zoom: u8,
    bounds: (f64, f64, f64, f64),
    provenance: Option<&Value>,
) -> Result<(), String> {
    spool.flush()?;
    let mut entries = std::mem::take(&mut spool.entries);
    if entries.is_empty() {
        return Err("No tiles generated".to_string());
    }

    entries.sort_by_key(|entry| entry.tile_id);
    let addressed_tiles = entries.len() as u64;
    let deduplicated = spool.dedup.is_some();
    let tile_contents = if deduplicated {
        spool.contents
    } else {
        addressed_tiles
    };
    if deduplicated {
        entries = merge_runs(entries);
    }

    // PMTiles v3 `clustered` requires the tile DATA to be laid out in
    // ascending tile_id order, not just the directory entries. A deduplicated
    // entry may point back at data already laid out.
    let clustered = is_clustered(&entries);

    let out_path = Path::new(output_path);
    let out_dir = match out_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let tmp_stem = format!(
        "{}.tmp",
        out_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("freestiler")
    );
    let (output, tmp_path) = create_exclusive_temp(out_dir, &tmp_stem, false)?;
    let mut guard = TempFileGuard {
        path: tmp_path.clone(),
        armed: true,
    };
    let mut output = BufWriter::new(output);

    output
        .seek(SeekFrom::Start(PMTILES_HEADER_BYTES))
        .map_err(|e| format!("Cannot seek output PMTiles header: {}", e))?;

    let root_directory_offset = PMTILES_HEADER_BYTES;
    let leaf_directories = write_directories(&mut output, &entries, PmCompression::GZip, None)
        .map_err(|e| format!("Cannot write PMTiles directory: {}", e))?;
    let root_directory_length = output
        .stream_position()
        .map_err(|e| format!("Cannot compute root directory length: {}", e))?
        - root_directory_offset;

    let json_metadata_offset = root_directory_offset + root_directory_length;
    let metadata_bytes = build_metadata_bytes_with_provenance(layers, provenance)?;
    output
        .write_all(&metadata_bytes)
        .map_err(|e| format!("Cannot write PMTiles metadata: {}", e))?;
    let json_metadata_length = metadata_bytes.len() as u64;

    let leaf_directories_offset = json_metadata_offset + json_metadata_length;
    output
        .write_all(&leaf_directories)
        .map_err(|e| format!("Cannot write PMTiles leaf directories: {}", e))?;
    let leaf_directories_length = leaf_directories.len() as u64;

    let tile_data_offset = leaf_directories_offset + leaf_directories_length;
    let mut spool_reader = File::open(&spool.path)
        .map_err(|e| format!("Cannot read tile spool {}: {}", spool.path.display(), e))?;
    let tile_data_length = copy(&mut spool_reader, &mut output)
        .map_err(|e| format!("Cannot copy PMTiles tile data: {}", e))?;

    let mut header = Header::default();
    header.root_directory_offset = root_directory_offset;
    header.root_directory_length = root_directory_length;
    header.json_metadata_offset = json_metadata_offset;
    header.json_metadata_length = json_metadata_length;
    header.leaf_directories_offset = leaf_directories_offset;
    header.leaf_directories_length = leaf_directories_length;
    header.tile_data_offset = tile_data_offset;
    header.tile_data_length = tile_data_length;
    header.num_addressed_tiles = addressed_tiles;
    header.num_tile_entries = entries.len() as u64;
    header.num_tile_content = tile_contents;
    header.clustered = clustered;
    header.internal_compression = PmCompression::GZip;
    header.tile_compression = PmCompression::GZip;
    header.tile_type = match format {
        TileFormat::Mvt => TileType::Mvt,
        TileFormat::Mlt => TileType::Mvt, // patched to MLT below
    };
    header.min_zoom = min_zoom;
    header.max_zoom = max_zoom;
    header.center_zoom = ((min_zoom as u16 + max_zoom as u16) / 2) as u8;
    header.min_pos.longitude = bounds.0;
    header.min_pos.latitude = bounds.1;
    header.max_pos.longitude = bounds.2;
    header.max_pos.latitude = bounds.3;
    header.center_pos.longitude = (bounds.0 + bounds.2) / 2.0;
    header.center_pos.latitude = (bounds.1 + bounds.3) / 2.0;

    output
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("Cannot seek PMTiles header start: {}", e))?;
    header
        .to_writer(&mut output)
        .map_err(|e| format!("Cannot write PMTiles header: {}", e))?;

    // For MLT format, patch the tile_type byte in the header.
    // PMTiles v3 header: byte 99 is tile_type.
    if format == TileFormat::Mlt {
        output
            .seek(SeekFrom::Start(99))
            .map_err(|e| format!("Seek error: {}", e))?;
        output
            .write_all(&[0x06])
            .map_err(|e| format!("Write error: {}", e))?;
    }

    let output = output
        .into_inner()
        .map_err(|e| format!("Cannot flush PMTiles output: {}", e))?;
    output
        .sync_all()
        .map_err(|e| format!("Cannot sync PMTiles output: {}", e))?;
    drop(output);

    fs::rename(&tmp_path, out_path).map_err(|e| {
        format!(
            "Cannot move {} to {}: {}",
            tmp_path.display(),
            output_path,
            e
        )
    })?;
    guard.armed = false;

    Ok(())
}

/// Merge consecutive tile IDs that share one payload into run-length entries.
/// `entries` is sorted by tile ID, each with run length 1.
fn merge_runs(entries: Vec<Entry>) -> Vec<Entry> {
    let mut merged: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(last) = merged.last_mut() {
            if last.offset == entry.offset
                && last.length == entry.length
                && last.tile_id + u64::from(last.run_length) == entry.tile_id
            {
                last.run_length += 1;
                continue;
            }
        }
        merged.push(entry);
    }
    merged
}

/// Whether tile data is laid out in tile ID order: each entry either starts
/// where the data laid out so far ends, or points back inside it (a
/// deduplicated tile).
fn is_clustered(entries: &[Entry]) -> bool {
    let mut end = 0u64;
    for entry in entries {
        if entry.offset == end {
            end += u64::from(entry.length);
        } else if entry.offset + u64::from(entry.length) > end {
            return false;
        }
    }
    true
}

#[cfg(test)]
fn build_metadata_bytes(layers: &[LayerMeta]) -> Result<Vec<u8>, String> {
    build_metadata_bytes_with_provenance(layers, None)
}
fn build_metadata_bytes_with_provenance(
    layers: &[LayerMeta],
    provenance: Option<&Value>,
) -> Result<Vec<u8>, String> {
    let vector_layers: Vec<Value> = layers
        .iter()
        .map(|l| {
            let mut fields = serde_json::Map::new();
            for (i, name) in l.property_names.iter().enumerate() {
                // TileJSON has no union type. In particular upstream cluster
                // abbreviations are integers below 1000 and strings above it.
                if l.property_types.get(i).map(String::as_str) == Some("mixed") {
                    continue;
                }
                let field_type = match l.property_types.get(i).map(String::as_str) {
                    Some("integer" | "integer_double" | "numeric" | "double" | "float") => "Number",
                    Some("logical" | "boolean" | "bool") => "Boolean",
                    _ => "String",
                };
                fields.insert(name.clone(), Value::String(field_type.to_string()));
            }
            let mut layer_json = json!({
                "id": l.name,
                "fields": fields,
                "field_order": l.property_names,
                "minzoom": l.min_zoom,
                "maxzoom": l.max_zoom
            });
            if let Some(ref gt) = l.geometry_type {
                layer_json["geometry_type"] = Value::String(gt.clone());
            }
            layer_json
        })
        .collect();

    let mut metadata = json!({
        "vector_layers": vector_layers
    });
    if let Some(value) = provenance {
        metadata["freestiler"] = value.clone();
    }

    let metadata_json =
        serde_json::to_vec(&metadata).map_err(|e| format!("Metadata JSON error: {}", e))?;
    gzip_compress(&metadata_json)
}

/// Create a uniquely named file in `dir`, exclusively (`create_new`), retrying
/// on name collisions. Returns the open handle and the path. With `private`,
/// the file is created owner-only (0600) on Unix; without it the process
/// umask applies, which is right for a temp file that will be renamed into
/// place as the final output.
fn create_exclusive_temp(dir: &Path, stem: &str, private: bool) -> Result<(File, PathBuf), String> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    for _ in 0..16 {
        let candidate = dir.join(format!("{}.{}", stem, unique_suffix()));
        match opts.open(&candidate) {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(format!(
                    "Cannot create temporary file {}: {}",
                    candidate.display(),
                    e
                ))
            }
        }
    }
    Err(format!(
        "Cannot create a unique temporary file in {}",
        dir.display()
    ))
}

pub(crate) fn unique_suffix() -> String {
    // Wall-clock resolution alone is not unique across parallel callers.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}_{}_{}", std::process::id(), nanos, sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coord(z: u8, x: u32, y: u32) -> TileCoord {
        TileCoord { z, x, y }
    }

    #[test]
    fn metadata_has_tilejson_types_and_original_field_order() {
        use std::io::Read;
        let mut layers = test_layers();
        layers[0].property_names = vec!["n", "label", "tie", "wide", "mean"]
            .into_iter()
            .map(str::to_string)
            .collect();
        layers[0].property_types = vec![
            "integer",
            "character",
            "logical",
            "integer_double",
            "double",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let compressed = build_metadata_bytes(&layers).unwrap();
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(compressed.as_slice())
            .read_to_string(&mut decoded)
            .unwrap();
        let meta: Value = serde_json::from_str(&decoded).unwrap();
        let layer = &meta["vector_layers"][0];
        assert_eq!(
            layer["fields"],
            json!({"n": "Number", "label": "String", "tie": "Boolean", "wide": "Number", "mean": "Number"})
        );
        assert_eq!(
            layer["field_order"],
            json!(["n", "label", "tie", "wide", "mean"])
        );
    }

    fn test_layers() -> Vec<LayerMeta> {
        vec![LayerMeta {
            name: "test".to_string(),
            property_names: vec![],
            property_types: vec![],
            min_zoom: 0,
            max_zoom: 2,
            geometry_type: None,
        }]
    }

    fn temp_output(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "freestiler_test_{}_{}.pmtiles",
            name,
            unique_suffix()
        ))
    }

    fn read_header(path: &Path) -> Header {
        let mut f = File::open(path).unwrap();
        Header::from_reader(&mut f).unwrap()
    }

    fn gunzip(data: &[u8]) -> Vec<u8> {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(data)
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn tile_id_ordered_spool_is_clustered() {
        let mut spool = TileSpool::new().unwrap();
        // Ascending tile_id: z0 (0,0) = 0, z1 (0,0) = 1, z1 (1,1) = 3
        for c in [coord(0, 0, 0), coord(1, 0, 0), coord(1, 1, 1)] {
            spool.write_tile(c, b"tile-bytes").unwrap();
        }
        let out = temp_output("clustered");
        write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mvt,
            &test_layers(),
            0,
            2,
            (0.0, 0.0, 1.0, 1.0),
        )
        .unwrap();

        let header = read_header(&out);
        assert!(header.clustered);
        assert_eq!(header.num_addressed_tiles, 3);
        assert_eq!(header.num_tile_entries, 3);
        assert_eq!(header.num_tile_content, 3);

        // The archive parses and round-trips tile content
        let f = File::open(&out).unwrap();
        let mut pm = pmtiles2::PMTiles::from_reader(f).unwrap();
        assert_eq!(pm.num_tiles(), 3);
        assert_eq!(
            gunzip(&pm.get_tile(1, 1, 1).unwrap().unwrap()),
            b"tile-bytes"
        );

        fs::remove_file(&out).unwrap();
    }

    #[test]
    fn out_of_order_spool_is_not_clustered() {
        let mut spool = TileSpool::new().unwrap();
        // Written in descending tile_id order: data layout is not clustered
        for c in [coord(1, 1, 1), coord(1, 0, 0), coord(0, 0, 0)] {
            spool.write_tile(c, b"tile-bytes").unwrap();
        }
        let out = temp_output("unclustered");
        write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mvt,
            &test_layers(),
            0,
            2,
            (0.0, 0.0, 1.0, 1.0),
        )
        .unwrap();

        let header = read_header(&out);
        assert!(!header.clustered);

        // Directory entries are still sorted and the archive still reads back
        let f = File::open(&out).unwrap();
        let mut pm = pmtiles2::PMTiles::from_reader(f).unwrap();
        assert_eq!(pm.num_tiles(), 3);
        assert_eq!(
            gunzip(&pm.get_tile(0, 0, 0).unwrap().unwrap()),
            b"tile-bytes"
        );

        fs::remove_file(&out).unwrap();
    }

    #[test]
    fn mlt_format_patches_tile_type_byte() {
        let mut spool = TileSpool::new().unwrap();
        spool.write_tile(coord(0, 0, 0), b"tile-bytes").unwrap();
        let out = temp_output("mlt");
        write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mlt,
            &test_layers(),
            0,
            0,
            (0.0, 0.0, 1.0, 1.0),
        )
        .unwrap();

        let bytes = fs::read(&out).unwrap();
        assert_eq!(bytes[99], 0x06);
        fs::remove_file(&out).unwrap();
    }

    #[test]
    fn existing_output_replaced_atomically() {
        let out = temp_output("replace");
        fs::write(&out, b"OLD CONTENT").unwrap();

        let mut spool = TileSpool::new().unwrap();
        spool.write_tile(coord(0, 0, 0), b"tile-bytes").unwrap();
        write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mvt,
            &test_layers(),
            0,
            0,
            (0.0, 0.0, 1.0, 1.0),
        )
        .unwrap();

        let header = read_header(&out);
        assert_eq!(header.num_addressed_tiles, 1);
        fs::remove_file(&out).unwrap();
    }

    #[test]
    fn failed_write_preserves_existing_output() {
        let out = temp_output("preserve");
        fs::write(&out, b"PRECIOUS").unwrap();

        // Empty spool → error, and the existing destination is untouched
        let mut spool = TileSpool::new().unwrap();
        let result = write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mvt,
            &test_layers(),
            0,
            0,
            (0.0, 0.0, 1.0, 1.0),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&out).unwrap(), b"PRECIOUS");
        fs::remove_file(&out).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn spool_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let spool = TileSpool::new().unwrap();
        let mode = fs::metadata(&spool.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn failure_during_assembly_cleans_up_and_preserves_destination() {
        let dir = std::env::temp_dir().join(format!("freestiler_test_fail_{}", unique_suffix()));
        fs::create_dir(&dir).unwrap();
        let out = dir.join("out.pmtiles");
        fs::write(&out, b"PRECIOUS").unwrap();

        let mut spool = TileSpool::new().unwrap();
        spool.write_tile(coord(0, 0, 0), b"tile-bytes").unwrap();
        // Delete the spool file so the tile-data copy fails after the temp
        // output has been created and the directories written into it.
        fs::remove_file(&spool.path).unwrap();

        let result = write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mvt,
            &test_layers(),
            0,
            0,
            (0.0, 0.0, 1.0, 1.0),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&out).unwrap(), b"PRECIOUS");

        // The failed temp output must not be left behind.
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n != "out.pmtiles")
            .collect();
        assert!(leftovers.is_empty(), "leftover files: {:?}", leftovers);

        fs::remove_file(&out).unwrap();
        fs::remove_dir(&dir).unwrap();
    }

    fn entry(tile_id: u64, offset: u64, length: u32) -> Entry {
        Entry {
            tile_id,
            offset,
            length,
            run_length: 1,
        }
    }

    #[test]
    fn runs_merge_only_consecutive_ids_with_one_payload() {
        let merged = merge_runs(vec![
            entry(1, 0, 10),
            entry(2, 0, 10),
            entry(3, 0, 10),
            entry(5, 0, 10),
            entry(6, 10, 4),
            entry(7, 0, 10),
        ]);
        let runs: Vec<(u64, u64, u32)> = merged
            .iter()
            .map(|e| (e.tile_id, e.offset, e.run_length))
            .collect();
        assert_eq!(runs, vec![(1, 0, 3), (5, 0, 1), (6, 10, 1), (7, 0, 1)]);
    }

    #[test]
    fn back_references_stay_clustered() {
        assert!(is_clustered(&[
            entry(1, 0, 10),
            entry(2, 10, 5),
            entry(3, 0, 10)
        ]));
        assert!(!is_clustered(&[entry(1, 10, 5), entry(2, 0, 10)]));
        assert!(!is_clustered(&[entry(1, 0, 10), entry(2, 20, 5)]));
    }

    #[test]
    fn deduplicating_spool_writes_each_payload_once() {
        let dir = std::env::temp_dir();
        let mut spool = TileSpool::new_in(&dir).unwrap();
        spool.set_deduplicate(true);
        let same = gzip_compress(b"same tile").unwrap();
        let other = gzip_compress(b"another tile").unwrap();
        spool.write_compressed_tile(coord(14, 0, 0), &same).unwrap();
        spool
            .write_compressed_tile(coord(14, 1, 0), &other)
            .unwrap();
        spool.write_compressed_tile(coord(14, 0, 1), &same).unwrap();
        spool.write_compressed_tile(coord(14, 1, 1), &same).unwrap();
        assert_eq!(spool.len(), 4);
        assert_eq!(spool.contents(), 2);
        let out = temp_output("dedup");
        write_pmtiles_from_spool(
            out.to_str().unwrap(),
            &mut spool,
            TileFormat::Mvt,
            &test_layers(),
            14,
            14,
            (0.0, 0.0, 1.0, 1.0),
        )
        .unwrap();
        let header = read_header(&out);
        assert_eq!(header.num_addressed_tiles, 4);
        assert_eq!(header.num_tile_content, 2);
        assert!(header.clustered);
        assert_eq!(header.tile_data_length, (same.len() + other.len()) as u64);
        let mut archive = pmtiles2::PMTiles::from_reader(File::open(&out).unwrap()).unwrap();
        for (x, y, want) in [(0, 0, &same), (1, 0, &other), (0, 1, &same), (1, 1, &same)] {
            assert_eq!(&archive.get_tile(x, y, 14).unwrap().unwrap(), want);
        }
        let _ = fs::remove_file(out);
    }

    #[test]
    fn spool_file_removed_on_drop() {
        let spool = TileSpool::new().unwrap();
        let path = spool.path.clone();
        assert!(path.exists());
        drop(spool);
        assert!(!path.exists());
    }
}
