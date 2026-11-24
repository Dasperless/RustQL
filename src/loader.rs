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
    /// Number of JSON syntax issues automatically repaired in memory (`0` if strictly valid JSON)
    pub autofixed_count: usize,
}

/// Metadata stored alongside a cached normalized NDJSON file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheMetadata {
    source_path: String,
    file_size: u64,
    modified_nanos: u128,
    shape: JsonShape,
    row_count: usize,
    #[serde(default)]
    autofixed_count: usize,
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

/// Format a [`serde_json::Error`] with a short surrounding context snippet from `content`.
pub fn format_serde_error_with_snippet(content: &str, err: &serde_json::Error) -> String {
    let base = err.to_string();
    let line_no = err.line();
    let col_no = err.column();
    if line_no == 0 {
        return base;
    }
    if let Some(line_str) = content.lines().nth(line_no.saturating_sub(1)) {
        let chars: Vec<char> = line_str.chars().collect();
        if !chars.is_empty() {
            let col_idx = col_no.saturating_sub(1).min(chars.len());
            let start = col_idx.saturating_sub(25);
            let end = (col_idx + 25).min(chars.len());
            let mut snippet = String::new();
            if start > 0 {
                snippet.push_str("...");
            }
            for &c in &chars[start..end] {
                if c.is_control() {
                    snippet.push(' ');
                } else {
                    snippet.push(c);
                }
            }
            if end < chars.len() {
                snippet.push_str("...");
            }
            let snippet_trimmed = snippet.trim();
            if !snippet_trimmed.is_empty() {
                return format!("{} (near `{}`)", base, snippet_trimmed);
            }
        }
    }
    base
}

/// Detect the shape of a JSON string slice without modifying any source data.
pub fn detect_shape_from_str(content: &str, path_label: &str) -> Result<JsonShape, LoaderError> {
    detect_shape_from_str_with_options(content, path_label, false)
}

/// Detect the shape of a JSON string slice, optionally applying in-memory JSON autofix on parse failure.
pub fn detect_shape_from_str_with_options(
    content: &str,
    path_label: &str,
    autofix: bool,
) -> Result<JsonShape, LoaderError> {
    match detect_shape_from_str_strict(content, path_label) {
        Ok(shape) => Ok(shape),
        Err(err) if autofix => {
            let (fixed, fixes) = autofix_json_str(content);
            if fixes > 0 {
                if let Ok(shape) = detect_shape_from_str_strict(&fixed, path_label) {
                    return Ok(shape);
                }
            }
            if try_autofix_ndjson_lines(content, path_label).is_some() {
                return Ok(JsonShape::Ndjson);
            }
            Err(err)
        }
        Err(err) => Err(err),
    }
}

fn detect_shape_from_str_strict(content: &str, path_label: &str) -> Result<JsonShape, LoaderError> {
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
                message: format_serde_error_with_snippet(trimmed, &e),
            })?;

            // Check if there is a second top-level value (which would make it NDJSON)
            if let Some(second) = stream.next() {
                second.map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: format_serde_error_with_snippet(trimmed, &e),
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
                    message: format_serde_error_with_snippet(trimmed, &e),
                })?;

            if let Some(second_res) = stream.next() {
                // Validate the second record parses cleanly -> NDJSON
                second_res.map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: format_serde_error_with_snippet(trimmed, &e),
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
        // Skip whitespace and count commas between elements
        let mut next_non_ws = None;
        let mut comma_count = 0usize;
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
                if b.is_ascii_whitespace() {
                    consumed += 1;
                } else if b == b',' {
                    comma_count += 1;
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
                if comma_count > 0 {
                    return Err(LoaderError::JsonParse {
                        path: path_label.to_string(),
                        message: "trailing comma in JSON array".to_string(),
                    });
                }
                reader.consume(1);
                break;
            }
            Some(_) => {
                if rows.is_empty() {
                    if comma_count > 0 {
                        return Err(LoaderError::JsonParse {
                            path: path_label.to_string(),
                            message: "expected value in JSON array".to_string(),
                        });
                    }
                } else if comma_count == 0 {
                    return Err(LoaderError::JsonParse {
                        path: path_label.to_string(),
                        message: "expected ',' or ']' in JSON array".to_string(),
                    });
                } else if comma_count > 1 {
                    return Err(LoaderError::JsonParse {
                        path: path_label.to_string(),
                        message: "expected value between commas in JSON array".to_string(),
                    });
                }
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

    // Verify no trailing non-whitespace characters follow the closing ']'
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
            if b.is_ascii_whitespace() {
                consumed += 1;
            } else {
                return Err(LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: "trailing characters after top-level JSON array".to_string(),
                });
            }
        }
        reader.consume(consumed);
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
        let val: Value = serde_json::from_slice(trimmed).map_err(|e| {
            let line_str = String::from_utf8_lossy(trimmed);
            LoaderError::JsonParse {
                path: path_label.to_string(),
                message: format!(
                    "Line {}: {}",
                    idx,
                    format_serde_error_with_snippet(&line_str, &e)
                ),
            }
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
    load_json_file_with_options(table_name, path, cache_dir, disable_cache, false)
}

/// Parse a JSON file from disk with optional in-memory JSON autofixing (`--autofix`).
/// Strictly read-only with respect to `path`: source files are never modified on disk.
pub fn load_json_file_with_options(
    table_name: &str,
    path: &Path,
    cache_dir: Option<&Path>,
    disable_cache: bool,
    autofix: bool,
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
        if let Some((shape, rows, autofixed_count)) =
            try_load_from_cache(cdir, path, file_size, modified_nanos, &path_label, autofix)
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
                autofixed_count,
            });
        }
    }

    let (shape, rows, autofixed_count) = match try_strict_stream_load(path, &path_label) {
        Ok((shape, rows)) => (shape, rows, 0),
        Err(LoaderError::Io { path, source }) => return Err(LoaderError::Io { path, source }),
        Err(strict_err) => {
            // Read file into memory to either apply in-memory autofix or produce exact file-level
            // line/column diagnostics with a context snippet.
            let mut file_reopen = File::open(path).map_err(|e| LoaderError::Io {
                path: path_label.clone(),
                source: e,
            })?;
            let mut content = String::new();
            match file_reopen.read_to_string(&mut content) {
                Ok(_) => parse_json_str_with_options(&content, &path_label, autofix)?,
                Err(e) => {
                    if !autofix {
                        return Err(LoaderError::Io {
                            path: path_label,
                            source: e,
                        });
                    }
                    // Attempt lossy UTF-8 decoding when autofix is enabled
                    let raw_bytes = fs::read(path).map_err(|io_err| LoaderError::Io {
                        path: path_label.clone(),
                        source: io_err,
                    })?;
                    let lossy = String::from_utf8_lossy(&raw_bytes);
                    match parse_json_str_with_options(&lossy, &path_label, true) {
                        Ok((s, r, fixes)) => (s, r, fixes.max(1)),
                        Err(_) => return Err(strict_err),
                    }
                }
            }
        }
    };

    // Write normalized NDJSON cache for fast subsequent loads (e.g. top-level arrays, nested objects, or autofixed files).
    // Skip re-serializing large (> 1 MB) strictly valid files that are already in NDJSON format on disk.
    if let Some(ref cdir) = effective_cache_dir {
        if shape != JsonShape::Ndjson || file_size <= 1_048_576 || autofixed_count > 0 {
            let _ = write_to_cache(
                cdir,
                path,
                file_size,
                modified_nanos,
                &shape,
                &rows,
                autofixed_count,
            );
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
        autofixed_count,
    })
}

/// Attempt strict streaming load of a JSON/NDJSON file without autofix.
fn try_strict_stream_load(
    path: &Path,
    path_label: &str,
) -> Result<(JsonShape, Vec<Map<String, Value>>), LoaderError> {
    let mut file = File::open(path).map_err(|e| LoaderError::Io {
        path: path_label.to_string(),
        source: e,
    })?;

    // Peek first non-whitespace byte to decide streaming strategy
    let mut first_byte = None;
    {
        let mut probe_buf = [0u8; 4096];
        loop {
            let n = file.read(&mut probe_buf).map_err(|e| LoaderError::Io {
                path: path_label.to_string(),
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
        path: path_label.to_string(),
        reason: "File is empty or contains only whitespace".to_string(),
    })?;

    file.seek(SeekFrom::Start(0)).map_err(|e| LoaderError::Io {
        path: path_label.to_string(),
        source: e,
    })?;

    match first_b {
        b'[' => {
            let reader = BufReader::with_capacity(256 * 1024, file);
            let parsed_rows = stream_parse_top_level_array(reader, path_label)?;
            Ok((JsonShape::TopLevelArray, parsed_rows))
        }
        b'{' => {
            let mut reader = BufReader::with_capacity(256 * 1024, file);
            let mut first_line = Vec::new();
            loop {
                first_line.clear();
                let n = read_bounded_line(&mut reader, &mut first_line, MAX_NDJSON_LINE_BYTES)
                    .map_err(|e| LoaderError::Io {
                        path: path_label.to_string(),
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
                        path: path_label.to_string(),
                        source: e,
                    })?;
                if n == 0 || !trim_ascii_whitespace(&second_line).is_empty() {
                    break;
                }
            }

            if let Ok(first_obj) = first_line_val {
                let second_line_trimmed = trim_ascii_whitespace(&second_line);
                if !second_line_trimmed.is_empty() {
                    let second_obj: Value =
                        serde_json::from_slice(second_line_trimmed).map_err(|e| {
                            let line_str = String::from_utf8_lossy(second_line_trimmed);
                            LoaderError::JsonParse {
                                path: path_label.to_string(),
                                message: format!(
                                    "Line 2: {}",
                                    format_serde_error_with_snippet(&line_str, &e)
                                ),
                            }
                        })?;
                    let mut rows = Vec::new();
                    rows.push(value_to_row_map(first_obj));
                    rows.push(value_to_row_map(second_obj));
                    let rest = stream_parse_ndjson(reader, path_label)?;
                    rows.extend(rest);
                    Ok((JsonShape::Ndjson, rows))
                } else {
                    extract_from_single_object(first_obj, path_label)
                }
            } else {
                let mut file_reopen = File::open(path).map_err(|e| LoaderError::Io {
                    path: path_label.to_string(),
                    source: e,
                })?;
                let mut content = String::new();
                file_reopen
                    .read_to_string(&mut content)
                    .map_err(|e| LoaderError::Io {
                        path: path_label.to_string(),
                        source: e,
                    })?;
                parse_json_str_strict(&content, path_label)
            }
        }
        _ => Err(LoaderError::UnsupportedShape {
            path: path_label.to_string(),
            reason: format!(
                "Top-level JSON primitive starting with '{}' is not supported as a table",
                first_b as char
            ),
        }),
    }
}

/// Parse a JSON string into `(JsonShape, Vec<Map<String, Value>>)`.
pub fn parse_json_str(
    content: &str,
    path_label: &str,
) -> Result<(JsonShape, Vec<Map<String, Value>>), LoaderError> {
    let (shape, rows, _) = parse_json_str_with_options(content, path_label, false)?;
    Ok((shape, rows))
}

/// Parsed JSON payload tuple: `(detected_shape, rows, autofixed_count)`.
pub type ParsedJsonPayload = (JsonShape, Vec<Map<String, Value>>, usize);

/// Parse a JSON string into `(JsonShape, Vec<Map<String, Value>>, autofixed_count)`,
/// optionally repairing malformed JSON syntax in memory when `autofix` is `true`.
pub fn parse_json_str_with_options(
    content: &str,
    path_label: &str,
    autofix: bool,
) -> Result<ParsedJsonPayload, LoaderError> {
    match parse_json_str_strict(content, path_label) {
        Ok((shape, rows)) => Ok((shape, rows, 0)),
        Err(strict_err) if autofix => {
            let (fixed, fix_count) = autofix_json_str(content);
            if fix_count > 0 {
                if let Ok((shape, rows)) = parse_json_str_strict(&fixed, path_label) {
                    return Ok((shape, rows, fix_count.max(1)));
                }
            }
            if let Some((shape, rows, line_fixes)) = try_autofix_ndjson_lines(content, path_label) {
                return Ok((shape, rows, line_fixes.max(1)));
            }
            // If autofix made changes, report any remaining unrecoverable parse error with context
            if fix_count > 0 {
                if let Err(fixed_err) = parse_json_str_strict(&fixed, path_label) {
                    // Prefer strict_err if fixed_err became UnsupportedShape, otherwise return detailed JsonParse
                    if matches!(fixed_err, LoaderError::JsonParse { .. })
                        && !matches!(strict_err, LoaderError::JsonParse { .. })
                    {
                        return Err(fixed_err);
                    }
                }
            }
            Err(strict_err)
        }
        Err(strict_err) => Err(strict_err),
    }
}

fn parse_json_str_strict(
    content: &str,
    path_label: &str,
) -> Result<(JsonShape, Vec<Map<String, Value>>), LoaderError> {
    let trimmed = content.trim();
    let shape = detect_shape_from_str_strict(trimmed, path_label)?;
    match &shape {
        JsonShape::TopLevelArray => {
            let val: Value = serde_json::from_str(trimmed).map_err(|e| LoaderError::JsonParse {
                path: path_label.to_string(),
                message: format_serde_error_with_snippet(trimmed, &e),
            })?;
            if let Value::Array(arr) = val {
                let rows = arr.into_iter().map(value_to_row_map).collect();
                Ok((shape, rows))
            } else {
                unreachable!()
            }
        }
        JsonShape::Ndjson => {
            let stream = serde_json::Deserializer::from_str(trimmed).into_iter::<Value>();
            let mut rows = Vec::new();
            for item in stream {
                let val = item.map_err(|e| LoaderError::JsonParse {
                    path: path_label.to_string(),
                    message: format_serde_error_with_snippet(trimmed, &e),
                })?;
                rows.push(value_to_row_map(val));
            }
            Ok((shape, rows))
        }
        JsonShape::NestedObjectArray { .. } | JsonShape::SingleObject => {
            let val: Value = serde_json::from_str(trimmed).map_err(|e| LoaderError::JsonParse {
                path: path_label.to_string(),
                message: format_serde_error_with_snippet(trimmed, &e),
            })?;
            extract_from_single_object(val, path_label)
        }
    }
}

/// Attempt line-by-line NDJSON autofix when a multi-line file has truncated individual lines.
fn try_autofix_ndjson_lines(content: &str, path_label: &str) -> Option<ParsedJsonPayload> {
    let mut rows = Vec::new();
    let mut total_fixes = 0usize;
    let mut in_block_comment = false;

    for raw_line in content.lines() {
        let mut line = raw_line.trim().trim_start_matches('\u{feff}').trim();
        if line.len() != raw_line.trim().len() {
            total_fixes += 1;
        }
        if in_block_comment {
            if let Some(idx) = line.find("*/") {
                in_block_comment = false;
                line = line[idx + 2..].trim();
            } else {
                continue;
            }
        }
        if line.starts_with("/*") && !line.contains("*/") {
            in_block_comment = true;
            total_fixes += 1;
            continue;
        }
        if line.is_empty() || line.starts_with("//") {
            if line.starts_with("//") {
                total_fixes += 1;
            }
            continue;
        }
        let (fixed_line, fixes) = autofix_json_str(line);
        let fixed_trimmed = fixed_line.trim();
        if fixed_trimmed.is_empty() {
            total_fixes += fixes;
            continue;
        }
        total_fixes += fixes;
        let stream = serde_json::Deserializer::from_str(fixed_trimmed).into_iter::<Value>();
        let mut parsed_any = false;
        for item in stream {
            match item {
                Ok(val) => {
                    if !val.is_object() && !val.is_array() {
                        return None;
                    }
                    rows.push(value_to_row_map(val));
                    parsed_any = true;
                }
                Err(_) => return None,
            }
        }
        if !parsed_any {
            return None;
        }
    }

    let _ = path_label;
    if rows.len() >= 2 {
        Some((JsonShape::Ndjson, rows, total_fixes.max(1)))
    } else {
        None
    }
}

/// Pure-Rust single-pass lexical and structural autofixer for malformed JSON and NDJSON strings.
///
/// Returns `(repaired_json_string, fix_count)` without modifying any external state.
///
/// Supported repairs:
/// 1. Missing or empty values after `:` (e.g. `{"a": , "b": 1}` or `{"a": }` -> `null`).
/// 2. Trailing, leading, or duplicate commas in objects and arrays (`{"a": 1,}`, `[1, , 2]`), plus missing commas between elements/properties.
/// 3. Non-standard literals (`NaN`, `Infinity`, `-Infinity`, `undefined`, `None`, `True`, `False`) and malformed numbers (`+42`, `.5`, `42.`, `0x1A`).
/// 4. Single-quoted strings (`'text'`), unquoted object keys (`{id: 1}`), and JS comments (`// ...`, `/* ... */`).
/// 5. Truncated / unclosed strings or containers at EOF, UTF-8 BOM (`\u{FEFF}`), and raw unescaped control characters inside strings.
pub fn autofix_json_str(input: &str) -> (String, usize) {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ObjStep {
        ExpectKeyOrEnd,
        ExpectColon,
        ExpectValue,
        AfterValue,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ArrStep {
        ExpectValueOrEnd,
        AfterValue,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Frame {
        Object { step: ObjStep, pending_comma: bool },
        Array { step: ArrStep, pending_comma: bool },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RootStep {
        ExpectValue,
        AfterValue,
    }

    struct Cursor<'a> {
        src: &'a str,
        pos: usize,
    }

    impl<'a> Cursor<'a> {
        fn peek(&self) -> Option<char> {
            self.src[self.pos..].chars().next()
        }

        fn next_char(&mut self) -> Option<char> {
            let ch = self.peek()?;
            self.pos += ch.len_utf8();
            Some(ch)
        }

        fn starts_with(&self, pat: &str) -> bool {
            self.src[self.pos..].starts_with(pat)
        }

        /// Check if the next non-whitespace, non-comment character is `:`.
        fn followed_by_colon(&self) -> bool {
            let mut p = self.pos;
            let bytes = self.src.as_bytes();
            while p < self.src.len() {
                if self.src[p..].starts_with("//") {
                    p += 2;
                    while p < bytes.len() && bytes[p] != b'\n' {
                        p += 1;
                    }
                    continue;
                }
                if self.src[p..].starts_with("/*") {
                    p += 2;
                    if let Some(end) = self.src[p..].find("*/") {
                        p += end + 2;
                    } else {
                        return false;
                    }
                    continue;
                }
                if let Some(ch) = self.src[p..].chars().next() {
                    if ch.is_whitespace() || ch == '\u{feff}' {
                        p += ch.len_utf8();
                        continue;
                    }
                    return ch == ':';
                }
                break;
            }
            false
        }

        /// Check if the line following a newline starts with `{"` or `{'` (indicating a new NDJSON object).
        fn next_line_starts_object(&self) -> bool {
            let mut p = self.pos;
            while let Some(ch) = self.src[p..].chars().next() {
                if ch == ' ' || ch == '\t' || ch == '\r' {
                    p += ch.len_utf8();
                } else {
                    break;
                }
            }
            self.src[p..].starts_with("{\"") || self.src[p..].starts_with("{'")
        }
    }

    fn before_starting_value(
        stack: &mut [Frame],
        root_step: &mut RootStep,
        out: &mut String,
        fixes: &mut usize,
    ) {
        match stack.last_mut() {
            Some(Frame::Array {
                step,
                pending_comma,
            }) => {
                if *pending_comma {
                    out.push(',');
                    *pending_comma = false;
                } else if *step == ArrStep::AfterValue {
                    out.push_str(", ");
                    *fixes += 1;
                }
                *step = ArrStep::ExpectValueOrEnd;
            }
            Some(Frame::Object {
                step,
                pending_comma,
            }) => {
                if *step == ObjStep::ExpectColon {
                    out.push_str(": ");
                    *fixes += 1;
                    *step = ObjStep::ExpectValue;
                }
                *pending_comma = false;
            }
            None => {
                if *root_step == RootStep::AfterValue && !out.ends_with('\n') {
                    out.push('\n');
                }
                *root_step = RootStep::ExpectValue;
            }
        }
    }

    fn after_finishing_value(stack: &mut [Frame], root_step: &mut RootStep) {
        match stack.last_mut() {
            Some(Frame::Object { step, .. }) => {
                if *step == ObjStep::ExpectValue {
                    *step = ObjStep::AfterValue;
                }
            }
            Some(Frame::Array { step, .. }) => {
                *step = ArrStep::AfterValue;
            }
            None => {
                *root_step = RootStep::AfterValue;
            }
        }
    }

    fn read_and_fix_string(
        cursor: &mut Cursor<'_>,
        quote: char,
        out: &mut String,
        fixes: &mut usize,
    ) {
        let _ = cursor.next_char(); // consume opening quote
        if quote == '\'' {
            *fixes += 1;
        }
        out.push('"');
        loop {
            match cursor.next_char() {
                None => {
                    // Unclosed string at EOF
                    *fixes += 1;
                    out.push('"');
                    break;
                }
                Some(c) if c == quote => {
                    out.push('"');
                    break;
                }
                Some('\\') => match cursor.next_char() {
                    None => {
                        *fixes += 1;
                        out.push_str("\\\\\"");
                        break;
                    }
                    Some(esc @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                        out.push('\\');
                        out.push(esc);
                    }
                    Some('u') => {
                        let rem = &cursor.src[cursor.pos..];
                        if rem.len() >= 4
                            && rem.as_bytes()[..4].iter().all(|b| b.is_ascii_hexdigit())
                        {
                            out.push_str("\\u");
                            out.push_str(&rem[..4]);
                            cursor.pos += 4;
                        } else {
                            *fixes += 1;
                            out.push_str("\\\\u");
                        }
                    }
                    Some('\'') => {
                        if quote == '"' {
                            *fixes += 1;
                        }
                        out.push('\'');
                    }
                    Some('\n') => {
                        *fixes += 1;
                        out.push_str("\\n");
                    }
                    Some('\r') => {
                        if cursor.peek() == Some('\n') {
                            let _ = cursor.next_char();
                        }
                        *fixes += 1;
                        out.push_str("\\n");
                    }
                    Some(other) => {
                        *fixes += 1;
                        out.push_str("\\\\");
                        out.push(other);
                    }
                },
                Some('"') => {
                    // Unescaped double quote inside a single-quoted string
                    out.push_str("\\\"");
                }
                Some('\n') => {
                    if cursor.next_line_starts_object() {
                        // Truncated string at end of an NDJSON/object line before the next record
                        *fixes += 1;
                        out.push('"');
                        out.push('\n');
                        break;
                    }
                    *fixes += 1;
                    out.push_str("\\n");
                }
                Some('\r') => {
                    if cursor.peek() == Some('\n')
                        && cursor.src[cursor.pos + 1..]
                            .trim_start_matches([' ', '\t', '\r'])
                            .starts_with("{\"")
                    {
                        let _ = cursor.next_char();
                        *fixes += 1;
                        out.push('"');
                        out.push('\n');
                        break;
                    }
                    *fixes += 1;
                    out.push_str("\\r");
                }
                Some('\t') => {
                    *fixes += 1;
                    out.push_str("\\t");
                }
                Some(c) if (c as u32) < 0x20 => {
                    use std::fmt::Write as _;
                    *fixes += 1;
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                Some(c) => {
                    out.push(c);
                }
            }
        }
    }

    fn is_valid_unquoted_key(token: &str) -> bool {
        !token.is_empty()
            && token
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '$' | '.' | '@'))
    }

    fn normalize_value_token(token: &str) -> (String, usize) {
        match token {
            "true" | "false" | "null" => return (token.to_string(), 0),
            "NaN" | "+NaN" | "-NaN" | "Infinity" | "+Infinity" | "-Infinity" | "inf" | "+inf"
            | "-inf" | "undefined" | "None" | "NULL" | "nil" => {
                return ("null".to_string(), 1);
            }
            "True" | "TRUE" => return ("true".to_string(), 1),
            "False" | "FALSE" => return ("false".to_string(), 1),
            _ => {}
        }

        if serde_json::from_str::<serde_json::Number>(token).is_ok() {
            return (token.to_string(), 0);
        }

        // Check hexadecimal integer (e.g. 0x1A, -0xFF)
        let (neg, rest) = if let Some(r) = token.strip_prefix('-') {
            (true, r)
        } else if let Some(r) = token.strip_prefix('+') {
            (false, r)
        } else {
            (false, token)
        };
        if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            if !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                if let Ok(val) = i128::from_str_radix(hex, 16) {
                    let signed = if neg { -val } else { val };
                    return (signed.to_string(), 1);
                }
            }
        }

        // Repair numbers with leading '+', leading '.', trailing '.', or leading zeros (e.g. +42, .5, -.5, 42., 007)
        if rest
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
            && rest.chars().any(|c| c.is_ascii_digit())
        {
            let mut candidate = rest.to_string();
            if candidate.starts_with('.') {
                candidate.insert(0, '0');
            }
            if candidate.ends_with('.') {
                candidate.push('0');
            }
            // Strip redundant leading zeros before integer digits (e.g. "007" -> "7", "00.5" -> "0.5")
            let int_end = candidate.find(['.', 'e', 'E']).unwrap_or(candidate.len());
            let int_part = &candidate[..int_end];
            if int_part.len() > 1
                && int_part.starts_with('0')
                && int_part.chars().all(|c| c.is_ascii_digit())
            {
                let stripped = int_part.trim_start_matches('0');
                let clean_int = if stripped.is_empty() { "0" } else { stripped };
                candidate = format!("{}{}", clean_int, &candidate[int_end..]);
            }
            if neg {
                candidate.insert(0, '-');
            }
            if serde_json::from_str::<serde_json::Number>(&candidate).is_ok() {
                return (candidate, 1);
            }
        }

        (token.to_string(), 0)
    }

    let mut cursor = Cursor { src: input, pos: 0 };
    let mut out = String::with_capacity(input.len() + 32);
    let mut fixes = 0usize;
    let mut stack: Vec<Frame> = Vec::new();
    let mut root_step = RootStep::ExpectValue;

    while let Some(ch) = cursor.peek() {
        // 1. UTF-8 BOM
        if ch == '\u{feff}' {
            let _ = cursor.next_char();
            fixes += 1;
            continue;
        }

        // 2. Whitespace
        if ch.is_whitespace() {
            let _ = cursor.next_char();
            if ch.is_ascii_whitespace() {
                out.push(ch);
            } else {
                out.push(' ');
                fixes += 1;
            }
            continue;
        }

        // 3. Single-line and block comments
        if cursor.starts_with("//") {
            cursor.pos += 2;
            while let Some(c) = cursor.peek() {
                if c == '\n' {
                    break;
                }
                let _ = cursor.next_char();
            }
            fixes += 1;
            continue;
        }
        if cursor.starts_with("/*") {
            cursor.pos += 2;
            fixes += 1;
            while cursor.pos < cursor.src.len() {
                if cursor.starts_with("*/") {
                    cursor.pos += 2;
                    break;
                }
                let _ = cursor.next_char();
            }
            continue;
        }

        // 4. Structural characters and values
        match ch {
            '{' => {
                // If an open Object was in AfterValue or ExpectKeyOrEnd (e.g. missing closing '}' before next object in array or NDJSON), auto-close it
                if let Some(Frame::Object {
                    step: ObjStep::AfterValue | ObjStep::ExpectKeyOrEnd,
                    pending_comma,
                }) = stack.last().copied()
                {
                    let has_keys_or_after = matches!(
                        stack.last(),
                        Some(Frame::Object {
                            step: ObjStep::AfterValue,
                            ..
                        })
                    ) || pending_comma;
                    if has_keys_or_after {
                        if pending_comma {
                            fixes += 1;
                        }
                        out.push('}');
                        fixes += 1;
                        let _ = stack.pop();
                        after_finishing_value(&mut stack, &mut root_step);
                    }
                }
                before_starting_value(&mut stack, &mut root_step, &mut out, &mut fixes);
                let _ = cursor.next_char();
                out.push('{');
                stack.push(Frame::Object {
                    step: ObjStep::ExpectKeyOrEnd,
                    pending_comma: false,
                });
            }
            '[' => {
                before_starting_value(&mut stack, &mut root_step, &mut out, &mut fixes);
                let _ = cursor.next_char();
                out.push('[');
                stack.push(Frame::Array {
                    step: ArrStep::ExpectValueOrEnd,
                    pending_comma: false,
                });
            }
            '}' => {
                let _ = cursor.next_char();
                if stack.is_empty() {
                    // Stray closing brace at root level
                    fixes += 1;
                    continue;
                }
                // If top of stack is an unclosed Array inside an Object, auto-close the Array(s) first
                if matches!(stack.last(), Some(Frame::Array { .. })) {
                    let has_enclosing_obj = stack.iter().any(|f| matches!(f, Frame::Object { .. }));
                    if has_enclosing_obj {
                        while let Some(Frame::Array { pending_comma, .. }) = stack.last().copied() {
                            if pending_comma {
                                fixes += 1;
                            }
                            out.push(']');
                            fixes += 1;
                            let _ = stack.pop();
                            after_finishing_value(&mut stack, &mut root_step);
                        }
                    } else if let Some(Frame::Array { pending_comma, .. }) = stack.pop() {
                        if pending_comma {
                            fixes += 1;
                        }
                        out.push(']');
                        fixes += 1;
                        after_finishing_value(&mut stack, &mut root_step);
                        continue;
                    }
                }
                if let Some(Frame::Object {
                    step,
                    pending_comma,
                }) = stack.pop()
                {
                    if step == ObjStep::ExpectValue {
                        out.push_str("null");
                        fixes += 1;
                    }
                    if pending_comma {
                        fixes += 1;
                    }
                    out.push('}');
                    after_finishing_value(&mut stack, &mut root_step);
                }
            }
            ']' => {
                let _ = cursor.next_char();
                if stack.is_empty() {
                    // Stray closing bracket at root level
                    fixes += 1;
                    continue;
                }
                // If top of stack is an unclosed Object inside an Array, auto-close the Object(s) first
                if matches!(stack.last(), Some(Frame::Object { .. })) {
                    let has_enclosing_arr = stack.iter().any(|f| matches!(f, Frame::Array { .. }));
                    if has_enclosing_arr {
                        while let Some(Frame::Object {
                            step,
                            pending_comma,
                        }) = stack.last().copied()
                        {
                            if step == ObjStep::ExpectValue {
                                out.push_str("null");
                                fixes += 1;
                            }
                            if pending_comma {
                                fixes += 1;
                            }
                            out.push('}');
                            fixes += 1;
                            let _ = stack.pop();
                            after_finishing_value(&mut stack, &mut root_step);
                        }
                    } else if let Some(Frame::Object {
                        step,
                        pending_comma,
                    }) = stack.pop()
                    {
                        if step == ObjStep::ExpectValue {
                            out.push_str("null");
                            fixes += 1;
                        }
                        if pending_comma {
                            fixes += 1;
                        }
                        out.push('}');
                        fixes += 1;
                        after_finishing_value(&mut stack, &mut root_step);
                        continue;
                    }
                }
                if let Some(Frame::Array { pending_comma, .. }) = stack.pop() {
                    if pending_comma {
                        fixes += 1;
                    }
                    out.push(']');
                    after_finishing_value(&mut stack, &mut root_step);
                }
            }
            ':' => {
                let _ = cursor.next_char();
                match stack.last_mut() {
                    Some(Frame::Object { step, .. }) => match *step {
                        ObjStep::ExpectColon => {
                            out.push(':');
                            *step = ObjStep::ExpectValue;
                        }
                        ObjStep::ExpectValue => {
                            // Duplicate colon `{"a":: 1}`
                            fixes += 1;
                        }
                        _ => {
                            out.push(':');
                        }
                    },
                    _ => {
                        out.push(':');
                    }
                }
            }
            ',' => {
                let _ = cursor.next_char();
                match stack.last_mut() {
                    Some(Frame::Object {
                        step,
                        pending_comma,
                    }) => match *step {
                        ObjStep::ExpectValue => {
                            // Missing value after colon before comma: `{"a": , "b": 1}`
                            out.push_str("null");
                            fixes += 1;
                            *step = ObjStep::ExpectKeyOrEnd;
                            *pending_comma = true;
                        }
                        ObjStep::AfterValue => {
                            *step = ObjStep::ExpectKeyOrEnd;
                            *pending_comma = true;
                        }
                        ObjStep::ExpectKeyOrEnd => {
                            // Leading comma `{, "a": 1}` or duplicate comma `{"a": 1,, "b": 2}`
                            fixes += 1;
                        }
                        ObjStep::ExpectColon => {
                            out.push(',');
                        }
                    },
                    Some(Frame::Array {
                        step,
                        pending_comma,
                    }) => match *step {
                        ArrStep::AfterValue => {
                            *step = ArrStep::ExpectValueOrEnd;
                            *pending_comma = true;
                        }
                        ArrStep::ExpectValueOrEnd => {
                            // Leading comma `[, 1]` or duplicate comma `[1, , 2]`
                            fixes += 1;
                        }
                    },
                    None => {
                        // Trailing comma after top-level NDJSON record
                        fixes += 1;
                    }
                }
            }
            '"' | '\'' => {
                let is_obj_key = matches!(
                    stack.last(),
                    Some(Frame::Object {
                        step: ObjStep::ExpectKeyOrEnd | ObjStep::AfterValue,
                        ..
                    })
                );
                if is_obj_key {
                    if let Some(Frame::Object {
                        step,
                        pending_comma,
                    }) = stack.last_mut()
                    {
                        if *pending_comma {
                            out.push(',');
                            *pending_comma = false;
                        } else if *step == ObjStep::AfterValue {
                            out.push_str(", ");
                            fixes += 1;
                        }
                    }
                    read_and_fix_string(&mut cursor, ch, &mut out, &mut fixes);
                    if let Some(Frame::Object { step, .. }) = stack.last_mut() {
                        *step = ObjStep::ExpectColon;
                    }
                } else {
                    before_starting_value(&mut stack, &mut root_step, &mut out, &mut fixes);
                    read_and_fix_string(&mut cursor, ch, &mut out, &mut fixes);
                    after_finishing_value(&mut stack, &mut root_step);
                }
            }
            _ => {
                // Scan bare token up to next delimiter, whitespace, or comment
                let start = cursor.pos;
                while let Some(c) = cursor.peek() {
                    if c.is_whitespace()
                        || matches!(
                            c,
                            '{' | '}' | '[' | ']' | ':' | ',' | '"' | '\'' | '\u{feff}'
                        )
                        || cursor.starts_with("//")
                        || cursor.starts_with("/*")
                    {
                        break;
                    }
                    let _ = cursor.next_char();
                }
                let token = &cursor.src[start..cursor.pos];

                let in_obj_key_slot = matches!(
                    stack.last(),
                    Some(Frame::Object {
                        step: ObjStep::ExpectKeyOrEnd | ObjStep::AfterValue,
                        ..
                    })
                );

                if in_obj_key_slot {
                    if cursor.followed_by_colon() && is_valid_unquoted_key(token) {
                        if let Some(Frame::Object {
                            step,
                            pending_comma,
                        }) = stack.last_mut()
                        {
                            if *pending_comma {
                                out.push(',');
                                *pending_comma = false;
                            } else if *step == ObjStep::AfterValue {
                                out.push_str(", ");
                                fixes += 1;
                            }
                            out.push('"');
                            out.push_str(token);
                            out.push('"');
                            fixes += 1;
                            *step = ObjStep::ExpectColon;
                        }
                    } else {
                        out.push_str(token);
                    }
                } else {
                    before_starting_value(&mut stack, &mut root_step, &mut out, &mut fixes);
                    let (normalized, tok_fixes) = normalize_value_token(token);
                    out.push_str(&normalized);
                    fixes += tok_fixes;
                    after_finishing_value(&mut stack, &mut root_step);
                }
            }
        }
    }

    // Auto-close any unclosed containers at EOF
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Object {
                step,
                pending_comma,
            } => {
                if pending_comma {
                    fixes += 1;
                }
                match step {
                    ObjStep::ExpectColon => {
                        out.push_str(": null}");
                        fixes += 2;
                    }
                    ObjStep::ExpectValue => {
                        out.push_str("null}");
                        fixes += 2;
                    }
                    ObjStep::ExpectKeyOrEnd | ObjStep::AfterValue => {
                        out.push('}');
                        fixes += 1;
                    }
                }
            }
            Frame::Array { pending_comma, .. } => {
                if pending_comma {
                    fixes += 1;
                }
                out.push(']');
                fixes += 1;
            }
        }
        after_finishing_value(&mut stack, &mut root_step);
    }

    (out, fixes)
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
    autofix: bool,
) -> Option<ParsedJsonPayload> {
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

    // Do not serve a cache entry that required --autofix when running in strict mode
    if meta.autofixed_count > 0 && !autofix {
        return None;
    }

    let data_file = File::open(&data_path).ok()?;
    let reader = BufReader::with_capacity(64 * 1024, data_file);
    let rows = stream_parse_ndjson(reader, path_label).ok()?;
    if rows.len() != meta.row_count {
        return None;
    }

    Some((meta.shape, rows, meta.autofixed_count))
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
    autofixed_count: usize,
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
        autofixed_count,
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
        let data =
            "{\"id\": 1, \"text\": \"hello 🦀 world\"}\n{\"id\": 2, \"text\": \"second line\"}\n";
        let reader = BufReader::with_capacity(16, data.as_bytes());
        let rows = stream_parse_ndjson(reader, "test.ndjson")
            .expect("should stream parse without UTF-8 boundary error");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["text"], "hello 🦀 world");
    }

    #[test]
    fn test_autofix_missing_value_at_line_1_col_397() {
        // Construct a single-line JSON array where a missing value occurs right around column 397
        let prefix = r#"[{"id": 1, "name": "Alice Vance", "department": "Platform", "role": "Staff Engineer", "location": "Seattle", "active": true}, {"id": 2, "name": "Bob Chen", "department": "Platform", "role": "Senior Engineer", "location": "Austin", "active": true}, {"id": 3, "name": "Clara Diaz", "department": "Product", "role": "Principal PM", "location": "New York", "active": "#;
        let broken = format!("{}, \"salary\": 145000}}]", prefix);

        // Strict parse must fail with "expected value at line 1 column ..."
        let strict_err = parse_json_str_with_options(&broken, "users.json", false).unwrap_err();
        let msg = strict_err.to_string();
        assert!(
            msg.contains("expected value at line 1 column"),
            "Expected 'expected value at line 1 column', got: {}",
            msg
        );
        assert!(
            msg.contains("(near `"),
            "Expected context snippet in error message, got: {}",
            msg
        );

        // With autofix enabled, it repairs the missing value to `null` and parses all 3 rows
        let (shape, rows, fixes) =
            parse_json_str_with_options(&broken, "users.json", true).unwrap();
        assert_eq!(shape, JsonShape::TopLevelArray);
        assert_eq!(rows.len(), 3);
        assert!(fixes >= 1);
        assert_eq!(rows[2]["active"], Value::Null);
        assert_eq!(rows[2]["salary"], serde_json::json!(145000));
    }

    #[test]
    fn test_autofix_all_supported_malformed_patterns() {
        let broken = "\u{feff}// Top comment\n\
            [\n\
              /* block comment */\n\
              {id: 1, 'name': 'Alice\\'s \"App\"', \"score\": NaN, \"ratio\": Infinity, \"neg\": -Infinity, \"undef\": undefined,},\n\
              {id: 2, 'name': \"Bob\", \"score\": +42, \"ratio\": .5, \"neg\": -.25, \"undef\": None, \"flag1\": True, \"flag2\": False, \"empty_end\": },\n\
              ,\n\
              {\"id\": 3, \"name\": \"Charlie\", \"tags\": [\"rust\", , \"sql\", ], \"truncated\": \"hello";

        // Strict mode must reject
        assert!(parse_json_str_with_options(broken, "broken.json", false).is_err());

        // Autofix mode must repair all issues in memory
        let (shape, rows, fixes) =
            parse_json_str_with_options(broken, "broken.json", true).unwrap();
        assert_eq!(shape, JsonShape::TopLevelArray);
        assert_eq!(rows.len(), 3);
        assert!(fixes >= 10);

        assert_eq!(rows[0]["id"], serde_json::json!(1));
        assert_eq!(rows[0]["name"], serde_json::json!("Alice's \"App\""));
        assert_eq!(rows[0]["score"], Value::Null);
        assert_eq!(rows[0]["ratio"], Value::Null);
        assert_eq!(rows[0]["neg"], Value::Null);
        assert_eq!(rows[0]["undef"], Value::Null);

        assert_eq!(rows[1]["id"], serde_json::json!(2));
        assert_eq!(rows[1]["score"], serde_json::json!(42));
        assert_eq!(rows[1]["ratio"], serde_json::json!(0.5));
        assert_eq!(rows[1]["neg"], serde_json::json!(-0.25));
        assert_eq!(rows[1]["undef"], Value::Null);
        assert_eq!(rows[1]["flag1"], serde_json::json!(true));
        assert_eq!(rows[1]["flag2"], serde_json::json!(false));
        assert_eq!(rows[1]["empty_end"], Value::Null);

        assert_eq!(rows[2]["id"], serde_json::json!(3));
        assert_eq!(rows[2]["tags"], serde_json::json!(["rust", "sql"]));
        assert_eq!(rows[2]["truncated"], serde_json::json!("hello"));
    }

    #[test]
    fn test_autofix_unrecoverable_json_fails_with_diagnostic() {
        let corrupted = r#"[{"id": 1, "value": @@@corrupted_token@@@}]"#;
        let err = parse_json_str_with_options(corrupted, "corrupted.json", true).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("line 1 column") && msg.contains("@@@corrupted_token@@@"),
            "Expected line/column and context snippet for unrecoverable JSON, got: {}",
            msg
        );
    }
}
