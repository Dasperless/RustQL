# Rustrix

**Rustrix** is a pure-Rust Terminal UI (TUI) and CLI for running SQL queries against JSON files (`[ ... ]` top-level arrays, NDJSON/JSON Lines, and deeply nested JSON objects).

Built with zero C/C++ dependencies and 100% permissive (`MIT` / `Apache-2.0`) licensing across the entire dependency tree.

---

## Features

- **Pure-Rust SQL Execution Engine** (`sqlparser` + `serde_json`):
  - `SELECT` projections, column aliases (`AS`), `*`, `table.*`, `DISTINCT`, `CASE WHEN`, `CAST`, arithmetic, and scalar functions (`LOWER`, `UPPER`, `LENGTH`, `COALESCE`, `ROUND`, `ABS`, `TRIM`, `CONCAT`).
  - `WHERE` filtering (`=`, `!=`, `<`, `<=`, `>`, `>=`, `AND`, `OR`, `NOT`, `LIKE`, `ILIKE`, `IN (...)`, `BETWEEN ... AND ...`, `IS NULL`, `IS NOT NULL`).
  - `GROUP BY` and `HAVING` with aggregate functions (`COUNT(*)`, `COUNT(col)`, `COUNT(DISTINCT col)`, `SUM`, `AVG`, `MIN`, `MAX`).
  - Multi-file `JOIN` (`INNER JOIN`, `LEFT JOIN`, `RIGHT JOIN`, `FULL OUTER JOIN`, `CROSS JOIN`) with `ON` or `USING`.
  - `ORDER BY` (single or multiple columns, `ASC`/`DESC`), `LIMIT`, and `OFFSET`.
  - **Nested Field Access & Array Unnesting**:
    - Traverse nested objects with standard dot paths (`user.address.city`, `warehouse.aisle`).
    - Index into arrays (`items[0].name`, `suppliers[0]`).
    - Lateral `UNNEST` in `FROM` (`SELECT u.name, tag FROM users u, UNNEST(u.tags) AS tag` or on object arrays `SELECT o.order_id, item.sku FROM orders o, UNNEST(o.items) AS item`).
    - Inline `UNNEST` in `SELECT` (`SELECT organization, UNNEST(features) AS feature FROM single_config`).
- **Auto-Detection of JSON Shapes**:
  - Automatically detects and displays whether an input file is:
    1. **Top-Level JSON Array** (`[ {...}, {...} ]`)
    2. **NDJSON / JSON Lines** (newline-delimited JSON records)
    3. **Nested Object Wrapping a Primary Array** (e.g., `{ "api_version": "2.1", "records": [ {...} ] }` — automatically extracts the `records` array)
    4. **Single Nested JSON Object** (treated as a 1-row record whose fields and nested arrays can be queried or unnested)
- **Streaming & Read-Only Disk Caching**:
  - Input files are **strictly read-only** and are never modified in place.
  - Top-level JSON arrays and NDJSON files are parsed with buffered streaming readers.
  - Converted/normalized NDJSON data is cached in the system temp directory (`/tmp/rustrix_cache`, keyed by canonical path, file size, and modification timestamp) so repeat queries and subsequent invocations skip re-parsing from scratch.
- **3-Pane Interactive Terminal UI (TUI)**:
  - **Left Sidebar**: Loaded tables, auto-detected JSON shape, row count, cache status, and an expandable recursive schema tree with inferred types (`Int`, `Float`, `String`, `Bool`, `Array<T>`, `Object`).
  - **Top Bar**: SQL Query Editor (with `Up`/`Down` query history) + live Row Filter box (`f`).
  - **Main Result Table**: Scrollable (vertical & horizontal), column-sortable (`s`), filterable, and JSON type/syntax color-coded table with a scrollable, syntax-highlighted **Row & Cell JSON Inspector** (`Enter` or `d`) for viewing large or deeply nested JSON values.

---

## Installation & Build

Requires a standard Rust toolchain (`cargo` and `rustc`). No C compiler or native system libraries are required.

```bash
cargo build --release
```

The compiled binary will be located at `./target/release/rustrix`.

Run the test suite:

```bash
cargo test
```

---

## Usage

### 1. Interactive TUI Mode

Pass one or more JSON files to launch the interactive TUI:

```bash
cargo run --release -- examples/users.json examples/orders.ndjson examples/nested_catalog.json
```

Each file's stem becomes its SQL table name:
- `examples/users.json` → `users`
- `examples/orders.ndjson` → `orders`
- `examples/nested_catalog.json` → `nested_catalog`
- `examples/single_config.json` → `single_config`

*(Note: When only a single file is loaded, you can also query it as `FROM data` in addition to its filename stem. You can also assign custom table aliases on the command line via `alias=path/to/file.json`.)*

#### TUI Keyboard Shortcuts & Guided SQL Assistant

| Key | Action |
| --- | --- |
| `/` or `e` | Focus the **SQL Query Input** bar (activates live syntax validation & autocomplete) |
| `Tab` (in SQL Input) | Accept the highlighted **autocomplete suggestion** (or switch panes when popup is closed) |
| `Up` / `Down` (in SQL Input) | Navigate **autocomplete suggestions** (use `Ctrl+Up` / `Ctrl+Down` or close popup with `Esc` for query history) |
| `Ctrl+Space` (in SQL Input) | Open context-aware SQL autocomplete dropdown at cursor |
| `Enter` (in SQL Input) | Execute the current SQL query |
| `Ctrl+U` | Clear the active input line |
| `w` (in Result Table) | ⚡ Open **Quick SQL Filter & Action menu** for the selected cell (`= value`, `!= value`, `ILIKE`, `>=`, `<=`, `IS NOT NULL`, `GROUP BY` count) |
| `t` or `Ctrl+T` | 📖 Open **SQL Recipe & Template Picker** pre-filled with your loaded table and column names |
| `w` / `g` (in Schema Browser) | Smartly append a `WHERE field = ...` filter (`w`) or execute a `GROUP BY field` count query (`g`) |
| `Enter` (in Schema Browser) | Execute `SELECT * FROM <table>` (on table header) or append field path to SQL query |
| `Tab` / `Shift+Tab` | Cycle focus across panes: **Result Table** → **SQL Input** → **Row Search** → **Schema Browser** |
| `Up`/`Down`/`Left`/`Right` or `h`/`j`/`k`/`l` | Navigate rows and columns in the **Result Table** |
| `PageUp` / `PageDown` | Scroll 15 rows up or down |
| `Home` (`g`) / `End` (`G`) | Jump to first / last row |
| `s` (in Result Table) | Sort table by currently selected column (`ASC ▲` → `DESC ▼` → `Unsorted`) |
| `f` (in Result Table) | Focus the live **Quick Row Search** input |
| `c` (in Result Table) | Clear active row filter and interactive column sort |
| `Enter` or `d` (in Result Table) | Open the **Row & Cell JSON Detail Inspector** modal (with JSON color coding, collapsible fields, & embedded JSON auto-formatting) |
| `j`/`k`/`PgUp`/`PgDn`/`g`/`G` (in JSON Inspector) | Move the line cursor (with auto-scrolling) through Selected Cell and Full Row JSON content |
| `Space` or `Enter` (in JSON Inspector) | Toggle collapse / expand (`▾` / `▸ { … N fields }` / `▸ [ … N items ]`) on the selected JSON field or container |
| `a` (in JSON Inspector) | Toggle collapse / expand all JSON fields in the inspector |
| `h`/`l` or `Left`/`Right` (in JSON Inspector) | Switch the inspected column cell live without closing the modal |
| `Esc` / `q` / `d` (in JSON Inspector) | Close the JSON Detail Inspector modal |
| `?` or `F1` | Open keyboard shortcuts Help modal |
| `q` or `Ctrl+C` | Quit Rustrix |

---

### 2. Non-Interactive CLI Mode (`-q` / `--query`)

Run a SQL query directly from the command line, print results, and exit:

```bash
# Default ASCII/Unicode box table output
rustrix -q "SELECT id, name, department, salary, address.city FROM users WHERE active = true ORDER BY salary DESC" examples/users.json

# GROUP BY with aggregates
rustrix -q "SELECT department, COUNT(*) AS headcount, AVG(salary) AS avg_salary, MIN(salary), MAX(salary) FROM users GROUP BY department ORDER BY headcount DESC" examples/users.json

# JOIN across multiple JSON files (top-level array + NDJSON)
rustrix -q "SELECT u.name, u.address.city AS city, o.order_id, o.total, o.status FROM users u JOIN orders o ON u.id = o.user_id WHERE o.status != 'cancelled' ORDER BY o.total DESC" examples/users.json examples/orders.ndjson

# Lateral UNNEST of nested arrays + 3-file JOIN
rustrix -q "SELECT o.order_id, item.sku, item.name, item.qty, c.warehouse.city AS warehouse_city FROM orders o, UNNEST(o.items) AS item JOIN nested_catalog c ON item.sku = c.sku ORDER BY o.order_id" examples/orders.ndjson examples/nested_catalog.json

# Output as JSON, NDJSON, or CSV (--format table|json|ndjson|csv)
rustrix -q "SELECT organization, cluster.primary_region AS region, UNNEST(features) AS feature FROM single_config" --format json examples/single_config.json
```

---

## Supported JSON Shapes & Known Limitations

### Supported JSON Shapes
1. **Top-Level Array (`examples/users.json`)**: A JSON array `[ {...}, {...} ]` where elements are JSON objects (or primitives, which are wrapped under a `value` column).
2. **NDJSON / JSON Lines (`examples/orders.ndjson`)**: One JSON value per line, or multiple concatenated top-level JSON objects.
3. **Nested Wrapper Object (`examples/nested_catalog.json`)**: A top-level JSON object `{ "api_version": "...", "records": [ {...}, {...} ] }` containing metadata fields alongside a primary array of object records. Rustrix auto-detects and extracts the largest object array (`records`) as the table rows.
4. **Single Top-Level Object (`examples/single_config.json`)**: A single JSON object without an array of object records. Treated as a 1-row table whose nested fields (`cluster.primary_region`) and arrays (`UNNEST(features)`) can be queried directly.

### Known Limitations
- **Top-Level Primitives**: A JSON file containing only a bare top-level scalar primitive (e.g., `42`, `"hello"`, `true`) or an empty file is rejected with a clear `UnsupportedShape` error because it does not represent structured record data.
- **SQL Subset**: Subqueries in `WHERE` (`WHERE id IN (SELECT ...)`), Common Table Expressions (`WITH ... AS`), Window Functions (`OVER (PARTITION BY ...)`), and data-mutating statements (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`) are intentionally unsupported because Rustrix is strictly read-only. Derived subqueries in `FROM (SELECT ...)` are supported.
- **Memory Model for Joins/Sorts**: While files are streamed during parsing and cached as normalized NDJSON on disk, executing full-table `JOIN`s, `GROUP BY`, and `ORDER BY` materializes the active working rows in memory.

---

## Dependency & License Audit

All direct and transitive dependencies in `Cargo.lock` are 100% pure Rust (no `-sys` C/C++ bindings) and licensed under `MIT` or `Apache-2.0`:

| Crate | Purpose | License |
| --- | --- | --- |
| `clap` | CLI argument parsing | `MIT OR Apache-2.0` |
| `crossterm` | Cross-platform terminal control | `MIT` |
| `ratatui` | Terminal UI widgets & layout | `MIT` |
| `serde` / `serde_json` | Streaming JSON parsing & serialization | `MIT OR Apache-2.0` |
| `sqlparser` | Pure-Rust ANSI/Generic SQL AST parser | `Apache-2.0` |
| `thiserror` | Structured error definitions | `MIT OR Apache-2.0` |
| `tempfile` (dev) | Temporary directories in tests | `MIT OR Apache-2.0` |

## License

This project is licensed under the **MIT License**. See [LICENSE](LICENSE) for full details.