//! # Rustrix
//!
//! **Rustrix** is a pure-Rust terminal UI (TUI) and command-line interface (CLI)
//! for running SQL queries directly against JSON and NDJSON files.
//!
//! ## Core Guarantees
//! - **Memory Safety**: Compiled with `#![forbid(unsafe_code)]` and zero C/C++ dependencies.
//! - **Read-Only Data Integrity**: Input JSON files are opened strictly in read-only mode and are never modified in place.
//! - **Multi-Shape JSON Auto-Detection**: Transparently queries top-level JSON arrays (`[...]`),
//!   NDJSON / JSON Lines, nested wrapper objects (`{ "records": [...] }`), and single nested JSON objects.
//!
//! ## Quick Start (Library Usage)
//! ```no_run
//! use rustrix::engine::QueryEngine;
//! use rustrix::loader::{load_json_file, parse_file_spec};
//!
//! let mut engine = QueryEngine::new();
//! let (table_name, path) = parse_file_spec("examples/users.json");
//! let table = load_json_file(&table_name, &path, None, false).expect("load JSON");
//! engine.register_table(table);
//!
//! let result = engine
//!     .execute("SELECT name, department, salary FROM users WHERE active = true ORDER BY salary DESC")
//!     .expect("execute SQL");
//! assert!(!result.rows.is_empty());
//! ```

#![forbid(unsafe_code)]

pub mod cli;
pub mod engine;
pub mod loader;
pub mod sql_helper;
pub mod tui;
