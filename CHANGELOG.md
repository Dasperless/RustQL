# Changelog

All notable changes to **RustQL** are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-20

### Added
- **Pure-Rust SQL Execution Engine** (`src/engine.rs`):
  - `SELECT` projections, column aliases (`AS`), `*`, `table.*`, `DISTINCT`, `CASE WHEN`, `CAST`, checked arithmetic, and scalar functions (`LOWER`, `UPPER`, `LENGTH`, `ARRAY_LENGTH`, `COALESCE`, `IFNULL`, `ROUND`, `ABS`, `TRIM`, `CONCAT`).
  - `WHERE` filtering (`=`, `!=`, `<`, `<=`, `>`, `>=`, `AND`, `OR`, `NOT`, `LIKE`, `ILIKE`, `IN (...)`, `BETWEEN ... AND ...`, `IS NULL`, `IS NOT NULL`).
  - `GROUP BY` and `HAVING` with aggregate functions (`COUNT(*)`, `COUNT(col)`, `COUNT(DISTINCT col)`, `SUM`, `AVG`, `MIN`, `MAX`).
  - Multi-file `JOIN` (`INNER JOIN`, `LEFT JOIN`, `RIGHT JOIN`, `FULL OUTER JOIN`, `CROSS JOIN`) with `ON` or `USING`.
  - Nested JSON field traversal via dot-paths (`user.address.city`), array indexing (`items[0].name`), lateral `UNNEST` in `FROM`, and inline `UNNEST` in `SELECT`.
  - `ORDER BY` (single/multi-column, `ASC`/`DESC`, positional index), `LIMIT`, and `OFFSET` with early scan short-circuiting.
- **Multi-Shape JSON Loader & Hardened Disk Cache** (`src/loader.rs`):
  - Automatic detection of Top-Level JSON Arrays (`[...]`), NDJSON / JSON Lines, Nested Object Wrappers (`{ "records": [...] }`), and Single Nested JSON Objects.
  - Buffered streaming readers with owner-only (`0700`/`0600`), symlink-resistant, atomic on-disk NDJSON caching and automatic LRU pruning.
- **Interactive 3-Pane Terminal UI (TUI)** (`src/tui.rs`, `src/sql_helper.rs`):
  - Virtualized result table capable of smooth scrolling and filtering over 50,000+ rows.
  - Guided SQL Assistant with live syntax/schema validation, `"Did you mean...?"` typo diagnostics, inline ghost-text completion, and context-aware autocomplete popup.
  - One-key Quick SQL Filter menu (`w`) from any table cell or schema field and interactive SQL Recipe & Template Picker (`t` / `Ctrl+T`).
  - Scrollable, JSON syntax-highlighted Row & Cell Detail Inspector (`Enter` / `d`) with interactive node folding (`Space` / `Enter` / `a`) and embedded JSON string auto-formatting.
- **Non-Interactive CLI Mode** (`src/cli.rs`):
  - Execute queries directly with `-q` / `--query` and output in `table`, `json`, `ndjson`, or `csv` formats.
- **Security, Memory Safety & Release Versioning**:
  - Enforced `#![forbid(unsafe_code)]`, terminal ANSI escape sanitization, checked arithmetic overflow handling, bounded working row/recursion limits, and RAII terminal state restoration on panic.
  - Embedded Windows PE Application Manifest (`asInvoker`, Windows 10/11 compatibility, UTF-8 codepage) and `VS_VERSION_INFO` metadata via zero-dependency `build.rs`.
  - Automated CI/CD and release workflows with Valgrind zero-memory-leak verification, `SHA256SUMS`, and GitHub Artifact Build Provenance Attestations.
