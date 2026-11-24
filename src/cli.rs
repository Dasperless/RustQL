//! CLI argument parsing and non-interactive output formatting for RustQL.

use clap::{Parser, ValueEnum};
use serde_json::Value;
use std::path::PathBuf;

use crate::engine::QueryResult;

/// Output formats supported in non-interactive (`-q`) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Unicode/ASCII formatted box table
    Table,
    /// Pretty-printed JSON array of objects
    Json,
    /// Newline-delimited JSON objects
    Ndjson,
    /// Comma-separated values (RFC 4180)
    Csv,
}

/// RustQL: A pure-Rust TUI and CLI for running SQL queries against JSON files.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "rustql",
    version,
    about = "Run SQL queries against JSON files (Top-level Arrays, NDJSON, Nested Objects)"
)]
pub struct CliArgs {
    /// Input JSON files (supports `file.json` or custom alias `alias=file.json`).
    #[arg(required = true)]
    pub files: Vec<String>,

    /// Optional SQL query to execute in non-interactive mode (prints result and exits).
    #[arg(short = 'q', long = "query")]
    pub query: Option<String>,

    /// Output format when running in non-interactive (`-q`) mode.
    #[arg(short = 'f', long = "format", value_enum, default_value = "table")]
    pub format: OutputFormat,

    /// Optional custom cache directory for normalized NDJSON cache files.
    #[arg(long = "cache-dir")]
    pub cache_dir: Option<PathBuf>,

    /// Disable on-disk caching (keep everything in-memory only).
    #[arg(long = "no-cache")]
    pub no_cache: bool,

    /// Automatically repair common JSON and NDJSON syntax errors in memory (source files remain read-only).
    #[arg(long = "autofix")]
    pub autofix: bool,
}

/// Format a [`QueryResult`] according to the requested [`OutputFormat`].
pub fn format_query_result(result: &QueryResult, format: OutputFormat) -> String {
    match format {
        OutputFormat::Table => format_as_table(result),
        OutputFormat::Json => format_as_json(result),
        OutputFormat::Ndjson => format_as_ndjson(result),
        OutputFormat::Csv => format_as_csv(result),
    }
}

/// Convert a JSON value into a concise display string for table/CSV cells.
pub fn value_to_display_string(val: &Value) -> String {
    match val {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if n.is_i64() || n.is_u64() {
                    n.to_string()
                } else if (f - f.round()).abs() < 1e-12 {
                    format!("{:.1}", f)
                } else {
                    // Trim trailing zeros for cleaner display up to 6 decimals
                    let s = format!("{:.6}", f);
                    let s = s.trim_end_matches('0').trim_end_matches('.');
                    s.to_string()
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => serde_json::to_string(val).unwrap_or_default(),
    }
}

/// Sanitize a string for safe single-line terminal rendering by stripping/replacing
/// ANSI escape codes and C0/C1 control characters.
pub fn sanitize_terminal_text(s: &str) -> String {
    if !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\r' | '\t' => out.push(' '),
            '\x1b' => out.push_str("\\x1b"),
            c if c.is_control() => {
                use std::fmt::Write;
                let _ = write!(out, "\\u{{{:04x}}}", c as u32);
            }
            other => out.push(other),
        }
    }
    out
}

fn format_as_table(result: &QueryResult) -> String {
    if result.columns.is_empty() {
        return "(empty result set)\n".to_string();
    }

    let safe_cols: Vec<String> = result
        .columns
        .iter()
        .map(|c| sanitize_terminal_text(c))
        .collect();
    let mut widths: Vec<usize> = safe_cols.iter().map(|c| c.chars().count()).collect();
    let mut rendered_rows: Vec<Vec<String>> = Vec::with_capacity(result.rows.len());

    for row in &result.rows {
        let mut rendered_row = Vec::with_capacity(safe_cols.len());
        for (i, val) in row.iter().enumerate() {
            let s = sanitize_terminal_text(&value_to_display_string(val));
            let len = s.chars().count();
            if i < widths.len() && len > widths[i] {
                widths[i] = len;
            }
            rendered_row.push(s);
        }
        rendered_rows.push(rendered_row);
    }

    let mut out = String::new();

    // Top border
    out.push('+');
    for w in &widths {
        out.push_str(&"-".repeat(*w + 2));
        out.push('+');
    }
    out.push('\n');

    // Header
    out.push('|');
    for (i, col) in safe_cols.iter().enumerate() {
        let pad = widths[i].saturating_sub(col.chars().count());
        out.push(' ');
        out.push_str(col);
        out.push_str(&" ".repeat(pad + 1));
        out.push('|');
    }
    out.push('\n');

    // Separator
    out.push('+');
    for w in &widths {
        out.push_str(&"=".repeat(*w + 2));
        out.push('+');
    }
    out.push('\n');

    // Rows
    for row in &rendered_rows {
        out.push('|');
        for (i, cell) in row.iter().enumerate() {
            let pad = widths[i].saturating_sub(cell.chars().count());
            out.push(' ');
            out.push_str(cell);
            out.push_str(&" ".repeat(pad + 1));
            out.push('|');
        }
        out.push('\n');
    }

    // Bottom border
    out.push('+');
    for w in &widths {
        out.push_str(&"-".repeat(*w + 2));
        out.push('+');
    }
    out.push('\n');

    out
}

fn format_as_json(result: &QueryResult) -> String {
    let mut arr = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut obj = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            obj.insert(col.clone(), val.clone());
        }
        arr.push(Value::Object(obj));
    }
    let mut s = serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_else(|_| "[]".into());
    s.push('\n');
    s
}

fn format_as_ndjson(result: &QueryResult) -> String {
    let mut out = String::new();
    for row in &result.rows {
        let mut obj = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            obj.insert(col.clone(), val.clone());
        }
        if let Ok(line) = serde_json::to_string(&Value::Object(obj)) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn format_as_csv(result: &QueryResult) -> String {
    let mut out = String::new();
    let escape_csv = |s: &str| -> String {
        if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };

    let header: Vec<String> = result.columns.iter().map(|c| escape_csv(c)).collect();
    out.push_str(&header.join(","));
    out.push('\n');

    for row in &result.rows {
        let cells: Vec<String> = row
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    escape_csv(&value_to_display_string(v))
                }
            })
            .collect();
        out.push_str(&cells.join(","));
        out.push('\n');
    }
    out
}
