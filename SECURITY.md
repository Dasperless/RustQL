# Security Policy

## Security Model & Guarantees

Rustrix is designed from the ground up for safe, deterministic local data inspection:

1. **100% Safe Rust (`#![forbid(unsafe_code)]`) & Zero C/C++ Dependencies**:
   - The `rustrix` library and binary are compiled with `#![forbid(unsafe_code)]`.
   - The entire dependency tree (`clap`, `crossterm`, `ratatui`, `serde`, `serde_json`, `sqlparser`, `thiserror`) is pure Rust with zero `-sys` native C/C++ bindings, eliminating buffer overflows, use-after-free, double-free, and C FFI memory corruption vulnerabilities.
2. **Strictly Read-Only Source Files & SQL Dialect**:
   - Input JSON and NDJSON files are opened strictly in read-only mode (`std::fs::File::open`) and are never modified in place.
   - The SQL execution engine permits only `SELECT` queries. Mutating statements (`INSERT`, `UPDATE`, `DELETE`, `DROP`, `CREATE`, `ALTER`) are rejected at the AST validation stage.
3. **Hardened On-Disk Cache**:
   - Normalized NDJSON cache files are stored with owner-only permissions (`0700` on the cache directory, `0600` on cache files on Unix).
   - Symbolic links (`is_symlink`) are explicitly rejected for both the cache directory and cache files to prevent symlink overwrite and cache-poisoning attacks on multi-user systems.
   - Cache updates are written atomically via process-isolated temporary files and `rename()`, with canonical path, byte-size, and nanosecond modification timestamp verification.
   - On-disk caching can be completely bypassed at any time using `--no-cache`.
4. **Terminal Escape Sequence Sanitization**:
   - Untrusted string values and column headers rendered in the TUI and CLI ASCII box tables are sanitized to strip/escape raw ANSI escape sequences (`\x1b`) and C0/C1 control characters (`\x00`..`\x1f`, `\x7f`..`\x9f`), preventing terminal escape sequence injection attacks.
5. **Bounded Resource Consumption & Arithmetic Safety**:
   - Input file size (2 GiB), per-line NDJSON length (128 MiB), intermediate `JOIN`/`UNNEST` working row counts (5,000,000 rows), TUI query history (500 entries), cache entry count (64 datasets), and JSON recursion depth (64 levels) are strictly bounded to prevent denial-of-service, stack overflow, and out-of-memory crashes.
   - All integer arithmetic operations (`+`, `-`, `*`, `/`, `%`, unary `-`, `ABS`, `SUM`) use checked arithmetic with automatic `f64` promotion or `NULL` evaluation on division/modulo by zero.
6. **Zero Memory Leaks**:
   - Every release and CI run is audited under Valgrind (`valgrind --leak-check=full --show-leak-kinds=all --error-exitcode=1`) to verify zero definitely, indirectly, or possibly leaked bytes.

## Supported Versions

| Version | Supported |
| ------- | --------- |
| `0.1.x` | ✅ Yes    |

## Verifying Release Binaries

Official release archives published on [GitHub Releases](https://github.com/Dasperless/Rustrix/releases) include:
- `SHA256SUMS` checksums for every target archive.
- Cryptographic GitHub Artifact Build Provenance Attestations verifiable via the GitHub CLI:
  ```bash
  gh attestation verify rustrix-v0.1.0-<target>.tar.gz --repo Dasperless/Rustrix
  ```
- Embedded Windows Application Manifest (`asInvoker`, Windows 10/11 compatibility, UTF-8 codepage) and `VS_VERSION_INFO` resource metadata on Windows (`x86_64-pc-windows-msvc`).
- Ad-hoc Mach-O code signatures (`codesign --verify`) on macOS (`aarch64-apple-darwin` and `x86_64-apple-darwin`).

## Reporting a Vulnerability

If you discover a security vulnerability in Rustrix, please report it responsibly:
- Open a private security advisory via [GitHub Security Advisories](https://github.com/Dasperless/Rustrix/security/advisories/new).
- Include a minimal reproduction query or sample JSON payload along with your OS and `rustrix --version` output.
- You will receive an initial response within 48 hours.
