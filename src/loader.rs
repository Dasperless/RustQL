//! Data loading, JSON shape auto-detection, schema inference, and disk/memory caching.
//!
//! All input files are treated as strictly read-only. Source files are never modified.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use thiserror::Error;

/// Errors that can occur during data loading, shape detection, or caching.
#[derive(Debug, Error)]
pub enum LoaderError {
    #[error("I/O error reading '{path}': {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("JSON parse error in '{path}': {message}")]
    JsonParse { path: String, message: String },

    #[error("Unsupported or empty JSON input in '{path}': {reason}")]
    UnsupportedShape { path: String, reason: String },
}

/// Detected shape of an input JSON file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsonShape {
    /// A top-level JSON array of records: `[ {...}, {...} ]`
    TopLevelArray,
    /// Newline-delimited JSON / JSON Lines: one JSON value per line
    Ndjson,
    /// A top-level JSON object containing an extracted array under `extracted_key`
    NestedObjectArray { extracted_key: String },
    /// A single top-level JSON object treated as a 1-row table
    SingleObject,
}

impl JsonShape {
    /// Human-readable description of the detected JSON shape.
    pub fn description(&self) -> String {
        match self {
            JsonShape::TopLevelArray => "Top-Level JSON Array ([...])".to_string(),
            JsonShape::Ndjson => "NDJSON / JSON Lines (newline-delimited)".to_string(),
            JsonShape::NestedObjectArray { extracted_key } => {
                format!("Nested Object (extracted array '{}')", extracted_key)
            }
            JsonShape::SingleObject => "Single Nested JSON Object (1-row record)".to_string(),
        }
    }
}

/// Inferred schema field entry (supports nested dot paths and types).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaField {
    /// Field path (e.g. `id`, `user.name`, `tags[]`)
    pub path: String,
    /// Human-readable inferred type (e.g. `Int`, `Float`, `String`, `Bool`, `Array`, `Object`)
    pub data_type: String,
    /// Depth level for tree display in TUI
    pub depth: usize,
}

/// A loaded dataset ready for SQL querying.
#[derive(Debug, Clone)]
pub struct LoadedTable {
    /// SQL table name (e.g. `users`)
    pub name: String,
    /// Original file path
    pub source_path: PathBuf,
    /// Detected JSON shape
    pub shape: JsonShape,
    /// Parsed rows (each row is a JSON Object map)
    pub rows: Arc<Vec<Map<String, Value>>>,
    /// Ordered top-level column names discovered across rows
    pub columns: Vec<String>,
    /// Detailed recursive schema tree (including nested dot-paths) for the TUI schema browser
    pub schema_fields: Vec<SchemaField>,
    /// Whether this dataset was loaded from the normalized on-disk cache
    pub loaded_from_cache: bool,
}

/// Metadata stored alongside a cached normalized NDJSON file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheMetadata {
    source_path: String,
    file_size: u64,
    modified_nanos: u128,
    shape: JsonShape,
    row_count: usize,
}

/// Parse a CLI file specification (`path/to/file.json` or `alias=path/to/file.json`).
pub fn parse_file_spec(spec: &str) -> (String, PathBuf) {
    if let Some((alias, path_str)) = spec.split_once('=') {
        let candidate_path = Path::new(path_str);
        if !alias.trim().is_empty() && (candidate_path.exists() || !Path::new(spec).exists()) {
            return (
                sanitize_table_name(alias.trim()),
                candidate_path.to_path_buf(),
            );
        }
    }
    let path = PathBuf::from(spec);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("table");
    (sanitize_table_name(stem), path)
}

/// Sanitize a file stem into a clean SQL identifier.
pub fn sanitize_table_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for (i, ch) in raw.chars().enumerate() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if i == 0 && ch.is_ascii_digit() {
                out.push('_');
            }
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "table".to_string()
    } else {
        out
    }
}

/// Detect the shape of a JSON string slice without modifying any source data.
pub fn detect_shape_from_str(content: &str, path_label: &str) -> Result<JsonShape, LoaderError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(LoaderError::UnsupportedShape {
            path: path_label.to_string(),
            reason: "File is empty or contains only whitespace".to_string(),
        });
    }

    let first_char = trimmed.chars().next().unwrap();

    match first_char {
        '[' => {
            // Verify it's a valid JSON array (or possibly NDJSON of arrays)
            let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
            let first = stream.next().ok_or_else(|| LoaderError::UnsupportedShape {
                path: path_label.to_string(),
                reason: "Empty input".to_string(),
            })?;
            let first_val = first.map_err(|e| LoaderError::JsonParse {
                path: path_label.to_string(),
                message: e.to_string(),
            })?;

            // Check if there is a second top-level value (which would make it NDJSON)
            if let Some(second) = stream.next() {
                second.map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;
                return Ok(JsonShape::Ndjson);
            }

            if let Value::Array(_) = first_val {
                Ok(JsonShape::TopLevelArray)
            } else {
                Err(LoaderError::UnsupportedShape {
                    path: path_label.to_string(),
                    reason: "Expected JSON array".to_string(),
                })
            }
        }
        '{' => {
            // Check whether it is NDJSON (multiple top-level JSON objects separated by newlines)
            // or a single top-level JSON object.
            let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
            let first = stream
                .next()
                .ok_or_else(|| LoaderError::UnsupportedShape {
                    path: path_label.to_string(),
                    reason: "Empty JSON input".to_string(),
                })?
                .map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;

            if let Some(second_res) = stream.next() {
                // Validate the second record parses cleanly -> NDJSON
                second_res.map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;
                return Ok(JsonShape::Ndjson);
            }

            // Single top-level object: inspect if it wraps a primary array of records
            if let Value::Object(map) = first {
                if let Some(key) = find_best_nested_array_key(&map) {
                    Ok(JsonShape::NestedObjectArray { extracted_key: key })
                } else {
                    Ok(JsonShape::SingleObject)
                }
            } else {
                Err(LoaderError::UnsupportedShape {
                    path: path_label.to_string(),
                    reason: "Expected JSON object".to_string(),
                })
            }
        }
        _ => Err(LoaderError::UnsupportedShape {
            path: path_label.to_string(),
            reason: format!(
                "Top-level JSON primitive starting with '{}' is not a table of records",
                first_char
            ),
        }),
    }
}

/// Given a top-level JSON object map, determine if it is a wrapper object around a primary data array
/// (e.g., `{ "meta": {...}, "items": [ {...}, {...} ] }`).
///
/// We extract a top-level array field as the table rows when:
/// 1. The object has an array field whose elements are JSON objects (or non-empty array of records)
///    and either:
///    - It is the largest array of objects in the wrapper, OR
///    - All fields in the object are metadata + that array (e.g., `data`, `items`, `records`, `results`, `rows`, `users`, `entries`, or an array of objects).
fn find_best_nested_array_key(map: &Map<String, Value>) -> Option<String> {
    let mut best_obj_array: Option<(String, usize)> = None;

    for (k, v) in map {
        if let Value::Array(arr) = v {
            let all_objects_or_empty = !arr.is_empty() && arr.iter().all(|el| el.is_object());
            if all_objects_or_empty {
                let len = arr.len();
                match &best_obj_array {
                    Some((_, best_len)) if len <= *best_len => {}
                    _ => best_obj_array = Some((k.clone(), len)),
                }
            }
        }
    }

    if let Some((key, len)) = best_obj_array {
        // Extract if the array has multiple object records, or if the top-level object doesn't look
        // like an entity row itself (e.g. has <= 4 scalar fields or uses common collection keys).
        let scalar_count = map
            .values()
            .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
            .count();
        if len >= 2 || scalar_count <= 2 {
            return Some(key);
        }
    }
    None
}

/// Convert any JSON [`Value`] element into a row [`Map<String, Value>`].
/// Objects become rows directly; primitives or arrays are wrapped in `{"value": val}`.
fn value_to_row_map(val: Value) -> Map<String, Value> {
    match val {
        Value::Object(map) => map,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    }
}

/// Stream-parse a top-level JSON array from a [`Read`] stream element by element without loading
/// the entire file into a single monolithic string when possible.
fn stream_parse_top_level_array<R: BufRead>(
    mut reader: R,
    path_label: &str,
) -> Result<Vec<Map<String, Value>>, LoaderError> {
    // Advance past leading whitespace and opening '['
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).map_err(|e| LoaderError::Io {
            path: path_label.to_string(),
            source: e,
        })?;
        if n == 0 {
            return Err(LoaderError::UnsupportedShape {
                path: path_label.to_string(),
                reason: "Unexpected EOF before '['".to_string(),
            });
        }
        if !byte[0].is_ascii_whitespace() {
            if byte[0] != b'[' {
                return Err(LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: format!("Expected '[', found '{}'", byte[0] as char),
                });
            }
            break;
        }
    }

    let mut rows = Vec::new();
    loop {
        // Skip whitespace and commas between elements
        let mut next_non_ws = None;
        loop {
            let buf = reader.fill_buf().map_err(|e| LoaderError::Io {
                path: path_label.to_string(),
                source: e,
            })?;
            if buf.is_empty() {
                break;
            }
            let mut consumed = 0;
            for &b in buf {
                if b.is_ascii_whitespace() || b == b',' {
                    consumed += 1;
                } else {
                    next_non_ws = Some(b);
                    break;
                }
            }
            reader.consume(consumed);
            if next_non_ws.is_some() {
                break;
            }
        }

        match next_non_ws {
            Some(b']') => {
                reader.consume(1);
                break;
            }
            Some(_) => {
                let mut de = serde_json::Deserializer::from_reader(&mut reader);
                let val = Value::deserialize(&mut de).map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;
                rows.push(value_to_row_map(val));
            }
            None => {
                return Err(LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: "Unexpected EOF inside JSON array (missing closing ']')".to_string(),
                });
            }
        }
    }

    Ok(rows)
}

/// Maximum supported input file size (2 GiB) to prevent out-of-memory denial of service.
const MAX_INPUT_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Maximum bytes allowed for a single NDJSON line (128 MiB) to prevent unbounded buffer growth.
const MAX_NDJSON_LINE_BYTES: usize = 128 * 1024 * 1024;

/// Maximum number of cached dataset pairs (`.meta.json` + `.ndjson`) retained in the cache directory.
const MAX_CACHE_ENTRIES: usize = 64;

/// Trim leading and trailing ASCII whitespace from a byte slice.
#[inline]
fn trim_ascii_whitespace(mut slice: &[u8]) -> &[u8] {
    while let Some((first, rest)) = slice.split_first() {
        if first.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    while let Some((last, rest)) = slice.split_last() {
        if last.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    slice
}

/// Read a single line from `reader` into `buf`, enforcing `max_bytes` per line.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    let mut total_read = 0usize;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(total_read);
        }
        let (consume_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };
        if total_read.saturating_add(consume_len) > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "Single line exceeds maximum allowed length of {} bytes",
                    max_bytes
                ),
            ));
        }
        buf.extend_from_slice(&available[..consume_len]);
        reader.consume(consume_len);
        total_read += consume_len;
        if found_newline {
            return Ok(total_read);
        }
    }
}

/// Stream-parse an NDJSON / JSON Lines reader line-by-line reusing a single bounded buffer.
fn stream_parse_ndjson<R: BufRead>(
    mut reader: R,
    path_label: &str,
) -> Result<Vec<Map<String, Value>>, LoaderError> {
    let mut rows = Vec::new();
    let mut line = Vec::with_capacity(4096);
    let mut idx = 0usize;
    loop {
        line.clear();
        let n = read_bounded_line(&mut reader, &mut line, MAX_NDJSON_LINE_BYTES).map_err(|e| {
            LoaderError::Io {
                path: path_label.to_string(),
                source: e,
            }
        })?;
        if n == 0 {
            break;
        }
        idx += 1;
        let trimmed = trim_ascii_whitespace(&line);
        if trimmed.is_empty() {
            continue;
        }
        let val: Value = serde_json::from_slice(trimmed).map_err(|e| LoaderError::JsonParse {
            path: path_label.to_string(),
            message: format!("Line {}: {}", idx, e),
        })?;
        rows.push(value_to_row_map(val));
        // Reclaim memory if a single unusually large line expanded the reusable buffer beyond 1 MiB
        if line.capacity() > 1_048_576 {
            line.clear();
            line.shrink_to(4096);
        }
    }
    Ok(rows)
}

/// Parse a JSON file from disk, using the on-disk normalized NDJSON cache if available and fresh.
/// Strictly read-only with respect to `path`.
pub fn load_json_file(
    table_name: &str,
    path: &Path,
    cache_dir: Option<&Path>,
    disable_cache: bool,
) -> Result<LoadedTable, LoaderError> {
    let path_label = path.display().to_string();
    let meta = fs::metadata(path).map_err(|e| LoaderError::Io {
        path: path_label.clone(),
        source: e,
    })?;
    let file_size = meta.len();
    if file_size > MAX_INPUT_FILE_BYTES {
        return Err(LoaderError::UnsupportedShape {
            path: path_label,
            reason: format!(
                "File size ({} bytes) exceeds maximum supported in-memory limit ({} bytes)",
                file_size, MAX_INPUT_FILE_BYTES
            ),
        });
    }
    let modified_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let effective_cache_dir = if disable_cache {
        None
    } else {
        Some(
            cache_dir
                .map(|p| p.to_path_buf())
                .unwrap_or_else(default_cache_dir),
        )
    };

    // Check if a valid cached NDJSON exists for this file state
    if let Some(ref cdir) = effective_cache_dir {
        if let Some((shape, rows)) =
            try_load_from_cache(cdir, path, file_size, modified_nanos, &path_label)
        {
            let (columns, schema_fields) = infer_columns_and_schema(&rows);
            return Ok(LoadedTable {
                name: table_name.to_string(),
                source_path: path.to_path_buf(),
                shape,
                rows: Arc::new(rows),
                columns,
                schema_fields,
                loaded_from_cache: true,
            });
        }
    }

    // Open source file in read-only mode
    let mut file = File::open(path).map_err(|e| LoaderError::Io {
        path: path_label.clone(),
        source: e,
    })?;

    // Peek first non-whitespace byte to decide streaming strategy
    let mut first_byte = None;
    {
        let mut probe_buf = [0u8; 4096];
        loop {
            let n = file.read(&mut probe_buf).map_err(|e| LoaderError::Io {
                path: path_label.clone(),
                source: e,
            })?;
            if n == 0 {
                break;
            }
            if let Some(&b) = probe_buf[..n].iter().find(|&&b| !b.is_ascii_whitespace()) {
                first_byte = Some(b);
                break;
            }
        }
    }

    let first_b = first_byte.ok_or_else(|| LoaderError::UnsupportedShape {
        path: path_label.clone(),
        reason: "File is empty or contains only whitespace".to_string(),
    })?;

    file.seek(SeekFrom::Start(0)).map_err(|e| LoaderError::Io {
        path: path_label.clone(),
        source: e,
    })?;

    let (shape, rows) = match first_b {
        b'[' => {
            // Stream-parse top-level array
            let reader = BufReader::with_capacity(256 * 1024, file);
            let parsed_rows = stream_parse_top_level_array(reader, &path_label)?;
            (JsonShape::TopLevelArray, parsed_rows)
        }
        b'{' => {
            // Check if the first line is a complete JSON object followed by another line starting with '{'
            let mut reader = BufReader::with_capacity(256 * 1024, file);
            let mut first_line = Vec::new();
            loop {
                first_line.clear();
                let n = read_bounded_line(&mut reader, &mut first_line, MAX_NDJSON_LINE_BYTES)
                    .map_err(|e| LoaderError::Io {
                        path: path_label.clone(),
                        source: e,
                    })?;
                if n == 0 || !trim_ascii_whitespace(&first_line).is_empty() {
                    break;
                }
            }

            let first_line_trimmed = trim_ascii_whitespace(&first_line);
            let first_line_val: Result<Value, _> = serde_json::from_slice(first_line_trimmed);
            let mut second_line = Vec::new();
            loop {
                second_line.clear();
                let n = read_bounded_line(&mut reader, &mut second_line, MAX_NDJSON_LINE_BYTES)
                    .map_err(|e| LoaderError::Io {
                        path: path_label.clone(),
                        source: e,
                    })?;
                if n == 0 || !trim_ascii_whitespace(&second_line).is_empty() {
                    break;
                }
            }

            if let Ok(first_obj) = first_line_val {
                let second_line_trimmed = trim_ascii_whitespace(&second_line);
                if !second_line_trimmed.is_empty() {
                    // Validate second line is also valid JSON -> NDJSON stream!
                    let second_obj: Value =
                        serde_json::from_slice(second_line_trimmed).map_err(|e| {
                            LoaderError::JsonParse {
                                path: path_label.clone(),
                                message: format!("Line 2: {}", e),
                            }
                        })?;
                    let mut rows = Vec::new();
                    rows.push(value_to_row_map(first_obj));
                    rows.push(value_to_row_map(second_obj));
                    let rest = stream_parse_ndjson(reader, &path_label)?;
                    rows.extend(rest);
                    (JsonShape::Ndjson, rows)
                } else {
                    // Single line, single object
                    extract_from_single_object(first_obj, &path_label)?
                }
            } else {
                // Multi-line JSON object (or multi-line stream of objects)
                let mut file_reopen = File::open(path).map_err(|e| LoaderError::Io {
                    path: path_label.clone(),
                    source: e,
                })?;
                let mut content = String::new();
                file_reopen
                    .read_to_string(&mut content)
                    .map_err(|e| LoaderError::Io {
                        path: path_label.clone(),
                        source: e,
                    })?;
                parse_json_str(&content, &path_label)?
            }
        }
        _ => {
            return Err(LoaderError::UnsupportedShape {
                path: path_label,
                reason: format!(
                    "Top-level JSON primitive starting with '{}' is not supported as a table",
                    first_b as char
                ),
            });
        }
    };

    // Write normalized NDJSON cache for fast subsequent loads (e.g. top-level arrays or nested objects).
    // Skip re-serializing large (> 1 MB) files that are already in NDJSON format on disk.
    if let Some(ref cdir) = effective_cache_dir {
        if shape != JsonShape::Ndjson || file_size <= 1_048_576 {
            let _ = write_to_cache(cdir, path, file_size, modified_nanos, &shape, &rows);
        }
    }

    let (columns, schema_fields) = infer_columns_and_schema(&rows);

    Ok(LoadedTable {
        name: table_name.to_string(),
        source_path: path.to_path_buf(),
        shape,
        rows: Arc::new(rows),
        columns,
        schema_fields,
        loaded_from_cache: false,
    })
}

/// Parse a JSON string into `(JsonShape, Vec<Map<String, Value>>)`.
pub fn parse_json_str(
    content: &str,
    path_label: &str,
) -> Result<(JsonShape, Vec<Map<String, Value>>), LoaderError> {
    let shape = detect_shape_from_str(content, path_label)?;
    match &shape {
        JsonShape::TopLevelArray => {
            let val: Value =
                serde_json::from_str(content.trim()).map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;
            if let Value::Array(arr) = val {
                let rows = arr.into_iter().map(value_to_row_map).collect();
                Ok((shape, rows))
            } else {
                unreachable!()
            }
        }
        JsonShape::Ndjson => {
            let stream = serde_json::Deserializer::from_str(content.trim()).into_iter::<Value>();
            let mut rows = Vec::new();
            for item in stream {
                let val = item.map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;
                rows.push(value_to_row_map(val));
            }
            Ok((shape, rows))
        }
        JsonShape::NestedObjectArray { .. } | JsonShape::SingleObject => {
            let val: Value =
                serde_json::from_str(content.trim()).map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: e.to_string(),
                })?;
            extract_from_single_object(val, path_label)
        }
    }
}

fn extract_from_single_object(
    val: Value,
    path_label: &str,
) -> Result<(JsonShape, Vec<Map<String, Value>>), LoaderError> {
    if let Value::Object(mut map) = val {
        if let Some(key) = find_best_nested_array_key(&map) {
            if let Some(Value::Array(arr)) = map.remove(&key) {
                let rows = arr.into_iter().map(value_to_row_map).collect();
                return Ok((JsonShape::NestedObjectArray { extracted_key: key }, rows));
            }
        }
        Ok((JsonShape::SingleObject, vec![map]))
    } else {
        Err(LoaderError::UnsupportedShape {
            path: path_label.to_string(),
            reason: "Expected JSON object".to_string(),
        })
    }
}

/// Infer ordered top-level columns and recursive dot-path schema fields from a list of rows.
pub fn infer_columns_and_schema(rows: &[Map<String, Value>]) -> (Vec<String>, Vec<SchemaField>) {
    let mut columns: Vec<String> = Vec::new();
    // Sample up to 200 rows for schema discovery
    let sample_limit = rows.len().min(200);
    for row in &rows[..sample_limit] {
        for key in row.keys() {
            if !columns.contains(key) {
                columns.push(key.clone());
            }
        }
    }

    let row_refs: Vec<&Map<String, Value>> = rows.iter().take(100).collect();
    let mut schema_fields = Vec::new();
    for col in &columns {
        collect_schema_for_key(&row_refs, col, col, 0, &mut schema_fields);
    }

    (columns, schema_fields)
}

fn collect_schema_for_key(
    rows: &[&Map<String, Value>],
    top_or_sub_key: &str,
    full_path: &str,
    depth: usize,
    out: &mut Vec<SchemaField>,
) {
    let sample_limit = rows.len().min(100);
    let mut sample_val: Option<&Value> = None;
    for row in &rows[..sample_limit] {
        if let Some(v) = row.get(top_or_sub_key) {
            if !v.is_null() {
                sample_val = Some(v);
                break;
            }
        }
    }

    let dtype = match sample_val {
        None => "Null".to_string(),
        Some(Value::Null) => "Null".to_string(),
        Some(Value::Bool(_)) => "Bool".to_string(),
        Some(Value::Number(n)) => {
            if n.is_i64() || n.is_u64() {
                "Int".to_string()
            } else {
                "Float".to_string()
            }
        }
        Some(Value::String(_)) => "String".to_string(),
        Some(Value::Array(arr)) => {
            let elem_type = arr.first().map(simple_value_type).unwrap_or("Any");
            format!("Array<{}>", elem_type)
        }
        Some(Value::Object(_)) => "Object".to_string(),
    };

    out.push(SchemaField {
        path: full_path.to_string(),
        data_type: dtype,
        depth,
    });

    // If it's a nested object, recursively collect subfields up to depth 3 without cloning maps
    if depth < 3 {
        let mut sub_rows: Vec<&Map<String, Value>> = Vec::new();
        for row in &rows[..sample_limit] {
            if let Some(Value::Object(sub_map)) = row.get(top_or_sub_key) {
                sub_rows.push(sub_map);
            }
        }
        if !sub_rows.is_empty() {
            let mut sub_keys: Vec<String> = Vec::new();
            for sr in &sub_rows {
                for k in sr.keys() {
                    if !sub_keys.contains(k) {
                        sub_keys.push(k.clone());
                    }
                    if sub_keys.len() >= 20 {
                        break;
                    }
                }
                if sub_keys.len() >= 20 {
                    break;
                }
            }
            for sk in sub_keys {
                let child_path = format!("{}.{}", full_path, sk);
                collect_schema_for_key(&sub_rows, &sk, &child_path, depth + 1, out);
            }
        }
    }
}

fn simple_value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Bool",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "Int"
            } else {
                "Float"
            }
        }
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(_) => "Object",
    }
}

fn default_cache_dir() -> PathBuf {
    std::env::temp_dir().join("rustql_cache")
}

fn canonical_path_string(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn cache_key_for(path: &Path, file_size: u64, modified_nanos: u128) -> String {
    let canonical = canonical_path_string(path);
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    file_size.hash(&mut hasher);
    modified_nanos.hash(&mut hasher);
    format!("{:016x}_{:x}", hasher.finish(), file_size)
}

/// Check if a filesystem path exists and is a symbolic link.
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

fn try_load_from_cache(
    cache_dir: &Path,
    source_path: &Path,
    file_size: u64,
    modified_nanos: u128,
    path_label: &str,
) -> Option<(JsonShape, Vec<Map<String, Value>>)> {
    if is_symlink(cache_dir) {
        return None;
    }
    let key = cache_key_for(source_path, file_size, modified_nanos);
    let meta_path = cache_dir.join(format!("{}.meta.json", key));
    let data_path = cache_dir.join(format!("{}.ndjson", key));

    if !meta_path.exists()
        || !data_path.exists()
        || is_symlink(&meta_path)
        || is_symlink(&data_path)
    {
        return None;
    }

    let meta_bytes = fs::read(&meta_path).ok()?;
    let meta: CacheMetadata = serde_json::from_slice(&meta_bytes).ok()?;
    let expected_source = canonical_path_string(source_path);
    if meta.file_size != file_size
        || meta.modified_nanos != modified_nanos
        || (meta.source_path != expected_source
            && meta.source_path != source_path.to_string_lossy())
    {
        return None;
    }

    let data_file = File::open(&data_path).ok()?;
    let reader = BufReader::with_capacity(64 * 1024, data_file);
    let rows = stream_parse_ndjson(reader, path_label).ok()?;
    if rows.len() != meta.row_count {
        return None;
    }

    Some((meta.shape, rows))
}

/// Create a file with owner-only (`0o600` on Unix) permissions, refusing symlinks.
fn create_private_file(path: &Path) -> std::io::Result<File> {
    if is_symlink(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Refusing to write to symbolic link in cache directory",
        ));
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn write_to_cache(
    cache_dir: &Path,
    source_path: &Path,
    file_size: u64,
    modified_nanos: u128,
    shape: &JsonShape,
    rows: &[Map<String, Value>],
) -> std::io::Result<()> {
    if cache_dir.exists() && is_symlink(cache_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Refusing to use symbolic link as cache directory",
        ));
    }
    fs::create_dir_all(cache_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(cache_dir, fs::Permissions::from_mode(0o700));
    }

    let key = cache_key_for(source_path, file_size, modified_nanos);
    let meta_path = cache_dir.join(format!("{}.meta.json", key));
    let data_path = cache_dir.join(format!("{}.ndjson", key));

    if is_symlink(&meta_path) || is_symlink(&data_path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Refusing to overwrite symbolic link in cache directory",
        ));
    }

    let pid = std::process::id();
    let tmp_data_path = cache_dir.join(format!(".{}.ndjson.tmp.{}", key, pid));
    let tmp_meta_path = cache_dir.join(format!(".{}.meta.json.tmp.{}", key, pid));

    let data_file = create_private_file(&tmp_data_path)?;
    let mut writer = BufWriter::with_capacity(64 * 1024, data_file);
    for row in rows {
        serde_json::to_writer(&mut writer, row)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    drop(writer);
    fs::rename(&tmp_data_path, &data_path)?;

    let meta = CacheMetadata {
        source_path: canonical_path_string(source_path),
        file_size,
        modified_nanos,
        shape: shape.clone(),
        row_count: rows.len(),
    };
    let meta_json = serde_json::to_vec(&meta)?;
    {
        let mut meta_file = create_private_file(&tmp_meta_path)?;
        meta_file.write_all(&meta_json)?;
        meta_file.flush()?;
    }
    fs::rename(&tmp_meta_path, &meta_path)?;

    prune_old_cache_entries(cache_dir);
    Ok(())
}

/// Bound disk usage in `cache_dir` by evicting the oldest cached dataset pairs when count exceeds `MAX_CACHE_ENTRIES`.
fn prune_old_cache_entries(cache_dir: &Path) {
    let entries = match fs::read_dir(cache_dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut meta_files: Vec<(PathBuf, u128)> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
            if name.ends_with(".meta.json") && !name.starts_with('.') {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                meta_files.push((p, mtime));
            }
        }
    }
    if meta_files.len() <= MAX_CACHE_ENTRIES {
        return;
    }
    meta_files.sort_by_key(|(_, mtime)| *mtime);
    let to_remove = meta_files.len() - MAX_CACHE_ENTRIES;
    for (meta_path, _) in meta_files.into_iter().take(to_remove) {
        if let Some(stem) = meta_path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".meta.json"))
        {
            let data_path = cache_dir.join(format!("{}.ndjson", stem));
            let _ = fs::remove_file(&meta_path);
            let _ = fs::remove_file(&data_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_top_level_array() {
        let json = r#"[{"id": 1, "name": "Alice"}, {"id": 2, "name": "Bob"}]"#;
        let shape = detect_shape_from_str(json, "test.json").unwrap();
        assert_eq!(shape, JsonShape::TopLevelArray);
    }

    #[test]
    fn test_detect_ndjson() {
        let ndjson = "{\"id\": 1, \"name\": \"Alice\"}\n{\"id\": 2, \"name\": \"Bob\"}\n";
        let shape = detect_shape_from_str(ndjson, "test.ndjson").unwrap();
        assert_eq!(shape, JsonShape::Ndjson);
    }

    #[test]
    fn test_detect_nested_object_array() {
        let nested = r#"{
            "status": "ok",
            "version": 1,
            "records": [
                {"id": 10, "val": "x"},
                {"id": 20, "val": "y"}
            ]
        }"#;
        let shape = detect_shape_from_str(nested, "nested.json").unwrap();
        assert_eq!(
            shape,
            JsonShape::NestedObjectArray {
                extracted_key: "records".to_string()
            }
        );
    }

    #[test]
    fn test_detect_single_object() {
        let single = r#"{
            "company": "Acme",
            "founded": 1999,
            "active": true,
            "headquarters": {"city": "Austin", "state": "TX"},
            "departments": ["Sales", "Engineering"]
        }"#;
        let shape = detect_shape_from_str(single, "single.json").unwrap();
        assert_eq!(shape, JsonShape::SingleObject);
    }

    #[test]
    fn test_reject_empty_or_primitive() {
        assert!(detect_shape_from_str("   ", "empty.json").is_err());
        assert!(detect_shape_from_str("42", "num.json").is_err());
        assert!(detect_shape_from_str("\"hello\"", "str.json").is_err());
    }

    #[test]
    fn test_ndjson_multibyte_utf8_across_chunk_boundary() {
        let data = "{\"id\": 1, \"text\": \"hello 🦀 world\"}\n{\"id\": 2, \"text\": \"second line\"}\n";
        let reader = BufReader::with_capacity(16, data.as_bytes());
        let rows = stream_parse_ndjson(reader, "test.ndjson")
            .expect("should stream parse without UTF-8 boundary error");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["text"], "hello 🦀 world");
    }
}
