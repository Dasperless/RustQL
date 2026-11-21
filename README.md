# RustQL

[![CI](https://github.com/Dasperless/RustQL/actions/workflows/ci.yml/badge.svg)](https://github.com/Dasperless/RustQL/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Dasperless/RustQL)](https://github.com/Dasperless/RustQL/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Pure Rust](https://img.shields.io/badge/Rust-100%25%20Safe%20(%23!%5Bforbid(unsafe__code)%5D)-orange.svg)](src/lib.rs)

**RustQL** is a fast, pure-Rust Terminal UI (TUI) and command-line interface (CLI) for running SQL queries directly against JSON and NDJSON files (`[ ... ]` top-level arrays, NDJSON/JSON Lines, nested wrapper objects, and single JSON records).

Built with `#![forbid(unsafe_code)]`, zero C/C++ dependencies, zero memory leaks (verified under Valgrind), and 100% permissive (`MIT` / `Apache-2.0`) licensing across the entire dependency tree.

---

## Features

- **Pure-Rust SQL Execution Engine** (`sqlparser` + `serde_json`):
  - `SELECT` projections, column aliases (`AS`), `*`, `table.*`, `DISTINCT`, `CASE WHEN`, `CAST`, checked arithmetic, and scalar functions (`LOWER`, `UPPER`, `LENGTH`, `LEN`, `ARRAY_LENGTH`, `COALESCE`, `IFNULL`, `ROUND`, `ABS`, `TRIM`, `CONCAT`).
  - `WHERE` filtering (`=`, `!=`, `<`, `<=`, `>`, `>=`, `AND`, `OR`, `NOT`, `LIKE`, `ILIKE`, `IN (...)`, `BETWEEN ... AND ...`, `IS NULL`, `IS NOT NULL`, single- or double-quoted string literals).
  - `GROUP BY` and `HAVING` with O(N) hash grouping and aggregate functions (`COUNT(*)`, `COUNT(col)`, `COUNT(DISTINCT col)`, `SUM`, `AVG`, `MIN`, `MAX`).
  - Multi-file `JOIN` (`INNER JOIN`, `LEFT JOIN`, `RIGHT JOIN`, `FULL OUTER JOIN`, `CROSS JOIN`) with `ON` or `USING`.
  - `ORDER BY` (single or multiple columns, `ASC`/`DESC`, positional column numbers), `LIMIT`, and `OFFSET` with early scan short-circuiting.
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
- **Streaming & Hardened Read-Only Disk Caching**:
  - Input files are **strictly read-only** and are never modified in place.
  - Top-level JSON arrays and NDJSON files are parsed with buffered streaming readers.
  - Normalized NDJSON cache files are stored in the system temp directory (`/tmp/rustql_cache`, keyed by canonical path, file size, and nanosecond modification timestamp) using owner-only permissions (`0700` directory / `0600` files on Unix), symlink rejection, atomic rename writes, and automatic LRU cache pruning.
- **3-Pane Interactive Terminal UI (TUI)**:
  - **Left Sidebar**: Loaded tables, auto-detected JSON shape, row count, cache status, and an expandable recursive schema tree with inferred types (`Int`, `Float`, `String`, `Bool`, `Array<T>`, `Object`).
  - **Top Bar**: Guided SQL Query Editor (with live syntax/schema validation, `"Did you mean...?"` typo diagnostics, inline ghost-text completion, context-aware autocomplete dropdown, and `Ctrl+Up`/`Ctrl+Down` query history) + live Row Search box (`f`).
  - **Main Result Table**: Virtualized (50,000+ rows at 60+ FPS), scrollable (vertical & horizontal), column-sortable (`s`), filterable, and JSON type/syntax color-coded table with a one-key **Quick SQL Filter menu** (`w`), **SQL Recipe & Template Picker** (`t` / `Ctrl+T`), and a scrollable, syntax-highlighted **Row & Cell JSON Detail Inspector** (`Enter` or `d`) with collapsible JSON fields (`Space` / `Enter` / `a`).

---

## Installation & Quick Start

### 1. Automated One-Line Installers (Recommended: Zero Warnings)

Install RustQL in seconds without manual unzipping, path configuration, or security warnings.

#### Windows (PowerShell / Windows Terminal)

Run in **PowerShell** or **Windows Terminal**:

```powershell
irm https://raw.githubusercontent.com/Dasperless/RustQL/main/install.ps1 | iex
```

> [!TIP]
> **Zero Warnings Guaranteed**: The installer installs to `%LOCALAPPDATA%\Programs\rustql` (requiring **zero administrator privileges** and **zero UAC elevation prompts**), automatically adds `rustql` to your User `PATH`, and strips the Windows "Mark of the Web" via `Unblock-File` so that **Microsoft Defender SmartScreen never blocks or warns** when launching `rustql.exe`.
>
> If your PowerShell restricts script execution, run with execution-policy bypass:
> ```powershell
> powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/Dasperless/RustQL/main/install.ps1 | iex"
> ```

#### Windows (Scoop Package Manager)

If you use [Scoop](https://scoop.sh/):

```powershell
scoop install https://raw.githubusercontent.com/Dasperless/RustQL/main/scoop/rustql.json
```

Scoop installs the binary, shims it into your `PATH`, and unblocks it without any SmartScreen warnings.

#### Linux & macOS

Run in `bash` or `zsh`:

```bash
curl -fsSL https://raw.githubusercontent.com/Dasperless/RustQL/main/install.sh | bash
```

Auto-detects your OS (Linux / macOS) and CPU architecture (`x86_64` / Apple Silicon `aarch64`), verifies the SHA256 checksum against GitHub Releases, installs the binary to `~/.local/bin` (or `/usr/local/bin`), and configures executable permissions.

---

### 2. Manual Prebuilt Binary Downloads

Download the prebuilt archive for your platform directly from [GitHub Releases](https://github.com/Dasperless/RustQL/releases):

| Platform | Architecture | Release Archive | Package Format |
| --- | --- | --- | --- |
| **Windows** | `x86_64` (MSVC) | `rustql-v0.1.0-x86_64-pc-windows-msvc.zip` | `.zip` with `rustql.exe` + `install.ps1` |
| **Linux (Static: Zero glibc dependency)** | `x86_64` (musl) | `rustql-v0.1.0-x86_64-unknown-linux-musl.tar.gz` | `.tar.gz` with standalone static `rustql` |
| **Linux (Dynamic)** | `x86_64` (glibc >= 2.31) | `rustql-v0.1.0-x86_64-unknown-linux-gnu.tar.gz` | `.tar.gz` with `rustql` + `install.sh` |
| **macOS** (Apple Silicon) | `aarch64` (`M1`–`M4`) | `rustql-v0.1.0-aarch64-apple-darwin.tar.gz` | `.tar.gz` with signed Mach-O binary |
| **macOS** (Intel) | `x86_64` | `rustql-v0.1.0-x86_64-apple-darwin.tar.gz` | `.tar.gz` with signed Mach-O binary |

---

### 3. Windows Setup & Zero-Warning Guide

Windows security features (Microsoft Defender SmartScreen, UAC, and ExecutionPolicy) may sometimes present warnings when downloading unsigned third-party tools from web browsers. RustQL is built and packaged with extensive mitigations so you can run it completely warning-free:

#### Preventing SmartScreen Warnings ("Windows protected your PC")
- **Why it happens**: When you download a `.zip` or `.exe` through a web browser (Edge, Chrome, Firefox), Windows attaches an NTFS alternate data stream called **Mark of the Web (Zone.Identifier = 3)**.
- **How to avoid it entirely**:
  1. **Use the PowerShell 1-liner**: `irm https://raw.githubusercontent.com/Dasperless/RustQL/main/install.ps1 | iex` (downloads directly via PowerShell and automatically executes `Unblock-File`).
  2. **Or unblock manually via PowerShell**:
     ```powershell
     # Unblock the downloaded zip BEFORE extracting:
     Unblock-File .\rustql-v0.1.0-x86_64-pc-windows-msvc.zip

     # Or unblock the extracted executable:
     Unblock-File .\rustql.exe
     ```
  3. **Or unblock via File Explorer**:
     - Right-click `rustql-v0.1.0-x86_64-pc-windows-msvc.zip` (or `rustql.exe`) $\rightarrow$ Click **Properties**.
     - In the **General** tab at the bottom, check the **Unblock** checkbox.
     - Click **Apply** and **OK**.
  4. **Or bypass the one-time SmartScreen popup**:
     - If the blue "Windows protected your PC" dialog appears, click **More info** $\rightarrow$ Click **Run anyway**.

#### Built-in Windows Hardening
Every official Windows release binary of RustQL includes:
- **Embedded Application Manifest**: Compiled with `<requestedExecutionLevel level="asInvoker" uiAccess="false"/>` so it **never requests administrator rights or triggers UAC prompts**.
- **Windows 10/11 Compatibility GUIDs**: Explicitly declares `<supportedOS>` compatibility with Windows 10 and Windows 11.
- **Native UTF-8 Codepage**: Embedded `<activeCodePage>UTF-8</activeCodePage>` ensures modern Unicode table borders and symbols render accurately without requiring manual terminal configuration.
- **Long Path Support**: Embedded `<longPathAware>true</longPathAware>` eliminates the legacy 260-character `MAX_PATH` file path limitation.
- **Embedded PE `VS_VERSION_INFO`**: Embeds company name, product version, description, and copyright metadata into the `.exe` header to pass antivirus heuristic inspections.

#### Recommended Terminal on Windows
- Use **[Windows Terminal](https://aka.ms/terminal)** (default in Windows 11; install via Microsoft Store or `winget install Microsoft.WindowsTerminal` in Windows 10) for true 24-bit color, UTF-8 box-drawing characters, and mouse-wheel scrolling.
- If using legacy `cmd.exe`, run `chcp 65001` before launching RustQL if Unicode table borders appear distorted.

#### PowerShell SQL Quoting Note
In PowerShell, double-quoted strings (`"..."`) interpret `$` as variable expressions (e.g. `$col`). When passing SQL queries on the command line via `-q`, **wrap the query in single quotes (`'...'`)**:
```powershell
rustql -q 'SELECT id, name, department, salary FROM users WHERE salary > 70000' examples\users.json
```
Both backslashes (`examples\users.json`) and forward slashes (`examples/users.json`) are fully supported.

---

### 4. Linux & macOS Setup Guide

#### Linux Setup
1. **Automated One-Line Installer**:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/Dasperless/RustQL/main/install.sh | bash
   ```
   *Auto-detects CPU architecture, verifies SHA256 checksum, installs to `~/.local/bin` (or `/usr/local/bin` if root), and verifies PATH.*

2. **Manual Prebuilt Binary Installation**:
   ```bash
   # Static binary (zero glibc dependency, works on Alpine, Debian, Ubuntu, Arch, RHEL, etc.):
   tar -xzf rustql-v0.1.0-x86_64-unknown-linux-musl.tar.gz
   sudo install -m 755 rustql-v0.1.0-x86_64-unknown-linux-musl/rustql /usr/local/bin/
   # Or install without sudo to ~/.local/bin:
   install -m 755 rustql-v0.1.0-x86_64-unknown-linux-musl/rustql ~/.local/bin/

   # Or dynamic glibc binary:
   tar -xzf rustql-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
   sudo install -m 755 rustql-v0.1.0-x86_64-unknown-linux-gnu/rustql /usr/local/bin/
   ```
3. Works out of the box in all terminal emulators: GNOME Terminal, Alacritty, Kitty, Konsole, foot, xterm, and `tmux`.

#### macOS Setup
Release binaries are ad-hoc signed (`codesign`). If you download via a browser (Safari/Chrome) and macOS flags the file:
```bash
xattr -d com.apple.quarantine ./rustql
```
The automated `install.sh` script clears this attribute automatically.

---

### 5. Checksum & Provenance Verification

- **Checksums (`SHA256SUMS`)**:
  ```bash
  # Linux
  sha256sum -c SHA256SUMS --ignore-missing

  # macOS
  shasum -a 256 -c SHA256SUMS --ignore-missing

  # Windows (PowerShell)
  (Get-FileHash -Algorithm SHA256 .\rustql-v0.1.0-x86_64-pc-windows-msvc.zip).Hash -eq (Get-Content .\rustql-v0.1.0-x86_64-pc-windows-msvc.zip.sha256).Split(" ")[0]
  ```
- **Cryptographic Build Provenance**:
  Verify the release archive's origin directly through GitHub CLI:
  ```bash
  gh attestation verify rustql-v0.1.0-x86_64-unknown-linux-gnu.tar.gz --repo Dasperless/RustQL
  ```

---

### 6. Build from Source with Cargo

Requires standard Rust toolchain (`cargo` and `rustc` >= 1.80). No C/C++ compiler or external system libraries are needed:

```bash
# Build optimized release binary
cargo build --release

# Binary is placed at:
./target/release/rustql --version

# Or install directly into ~/.cargo/bin:
cargo install --locked --path .
```

Run the full test suite:

```bash
cargo test
```

---

## Usage

### 1. Interactive TUI Mode

Pass one or more JSON files to launch the interactive TUI:

```bash
rustql examples/users.json examples/orders.ndjson examples/nested_catalog.json
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
| `q` or `Ctrl+C` | Quit RustQL |

---

### 2. Non-Interactive CLI Mode (`-q` / `--query`)

Run a SQL query directly from the command line, print results, and exit:

```bash
# Default ASCII/Unicode box table output
rustql -q "SELECT id, name, department, salary, address.city FROM users WHERE active = true ORDER BY salary DESC" examples/users.json

# GROUP BY with aggregates
rustql -q "SELECT department, COUNT(*) AS headcount, AVG(salary) AS avg_salary, MIN(salary), MAX(salary) FROM users GROUP BY department ORDER BY headcount DESC" examples/users.json

# JOIN across multiple JSON files (top-level array + NDJSON)
rustql -q "SELECT u.name, u.address.city AS city, o.order_id, o.total, o.status FROM users u JOIN orders o ON u.id = o.user_id WHERE o.status != 'cancelled' ORDER BY o.total DESC" examples/users.json examples/orders.ndjson

# Lateral UNNEST of nested arrays + 3-file JOIN
rustql -q "SELECT o.order_id, item.sku, item.name, item.qty, c.warehouse.city AS warehouse_city FROM orders o, UNNEST(o.items) AS item JOIN nested_catalog c ON item.sku = c.sku ORDER BY o.order_id" examples/orders.ndjson examples/nested_catalog.json

# Output as JSON, NDJSON, or CSV (--format table|json|ndjson|csv)
rustql -q "SELECT organization, cluster.primary_region AS region, UNNEST(features) AS feature FROM single_config" --format json examples/single_config.json
```

---

### 3. Rust Library Usage

`rustql` can also be embedded as a library in Rust applications:

```rust
use rustql::engine::QueryEngine;
use rustql::loader::{load_json_file, parse_file_spec};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut engine = QueryEngine::new();
    let (table_name, path) = parse_file_spec("examples/users.json");
    let table = load_json_file(&table_name, &path, None, false)?;
    engine.register_table(table);

    let result = engine.execute(
        "SELECT department, COUNT(*) AS headcount, AVG(salary) AS avg_salary \
         FROM users WHERE active = true GROUP BY department ORDER BY headcount DESC",
    )?;

    for row in &result.rows {
        println!("{:?}", row);
    }
    Ok(())
}
```

---

## Security & Memory Safety

See [SECURITY.md](SECURITY.md) for the complete security architecture:

- **100% Safe Rust**: Enforced via `#![forbid(unsafe_code)]` in both `src/lib.rs` and `src/main.rs` with zero C/C++ FFI dependencies.
- **Zero Memory Leaks**: Audited with `valgrind --leak-check=full --show-leak-kinds=all --error-exitcode=1` in CI across SQL queries, multi-table joins, unnesting, and caching.
- **Strictly Read-Only Source Data**: Input JSON files are opened read-only and never modified. Data-mutating SQL statements (`INSERT`, `UPDATE`, `DELETE`, `DROP`) are rejected.
- **Hardened Disk Cache**: Cache directories are created with `0700` owner-only permissions (`0600` on cache files), reject symbolic links, write atomically via temporary files, verify canonical paths, and automatically prune beyond 64 cached datasets.
- **Terminal Escape Injection Protection**: Control characters and ANSI escape sequences (`\x1b`) inside untrusted JSON strings are sanitized before terminal table rendering.
- **Overflow & DoS Protection**: Checked integer arithmetic with `f64` overflow promotion, bounded intermediate join/unnest row limits (`5,000,000` rows), bounded JSON recursion depth (`64` levels), and RAII terminal raw-mode recovery on exit or panic.

---

## Supported JSON Shapes & Known Limitations

### Supported JSON Shapes
1. **Top-Level Array (`examples/users.json`)**: A JSON array `[ {...}, {...} ]` where elements are JSON objects (or primitives, which are wrapped under a `value` column).
2. **NDJSON / JSON Lines (`examples/orders.ndjson`)**: One JSON value per line, or multiple concatenated top-level JSON objects.
3. **Nested Wrapper Object (`examples/nested_catalog.json`)**: A top-level JSON object `{ "api_version": "...", "records": [ {...}, {...} ] }` containing metadata fields alongside a primary array of object records. RustQL auto-detects and extracts the largest object array (`records`) as the table rows.
4. **Single Top-Level Object (`examples/single_config.json`)**: A single JSON object without an array of object records. Treated as a 1-row table whose nested fields (`cluster.primary_region`) and arrays (`UNNEST(features)`) can be queried directly.

### Known Limitations
- **Top-Level Primitives**: A JSON file containing only a bare top-level scalar primitive (e.g., `42`, `"hello"`, `true`) or an empty file is rejected with a clear `UnsupportedShape` error because it does not represent structured record data.
- **SQL Subset**: Subqueries in `WHERE` (`WHERE id IN (SELECT ...)`), Common Table Expressions (`WITH ... AS`), Window Functions (`OVER (PARTITION BY ...)`), and data-mutating statements (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`) are intentionally unsupported because RustQL is strictly read-only. Derived subqueries in `FROM (SELECT ...)` are supported.
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

## Contributing & Changelog

- Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) before submitting a pull request.
- See [CHANGELOG.md](CHANGELOG.md) for release notes and version history.

## License

This project is licensed under the **MIT License**. See [LICENSE](LICENSE) for full details.