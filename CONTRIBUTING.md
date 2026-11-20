# Contributing to RustQL

Thank you for your interest in contributing to **RustQL**!

## Development Prerequisites

- A standard Rust toolchain (`rustup`, `cargo`, `rustc` >= 1.80) with `clippy` and `rustfmt`.
- No C/C++ compiler or native system libraries are required.
- Optional (Linux): `valgrind` for running memory leak verification locally.

## Building & Testing Locally

```bash
# 1. Clone the repository
git clone https://github.com/Dasperless/RustQL.git
cd RustQL

# 2. Run the full test suite (unit, integration, and doc-tests)
cargo test

# 3. Run Clippy with warnings treated as errors
cargo clippy --all-targets -- -D warnings

# 4. Verify rustdoc builds cleanly without warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# 5. Build the optimized release binary
cargo build --release
```

## Memory Leak Verification (Valgrind)

To run the Valgrind memory leak audit locally on Linux:

```bash
valgrind --leak-check=full --show-leak-kinds=all --errors-for-leak-kinds=definite,indirect,possible --error-exitcode=1 \
  ./target/release/rustql -q "SELECT u.name, o.order_id, item.sku FROM users u JOIN orders o ON u.id = o.user_id, UNNEST(o.items) AS item ORDER BY o.order_id" \
  examples/users.json examples/orders.ndjson
```

## Architectural Constraints

1. **No `unsafe` Code**: `#![forbid(unsafe_code)]` is enforced across the library and binary.
2. **Pure Rust & Permissive Licenses Only**: All dependencies must be pure Rust (no `-sys` C/C++ crates) and licensed under `MIT` or `Apache-2.0`.
3. **Strictly Read-Only Source Data**: Input JSON/NDJSON files must never be mutated in place.
4. **Zero Compiler or Clippy Warnings**: All pull requests must pass `cargo clippy --all-targets -- -D warnings` and `cargo test`.

## Pull Request Process

1. Fork the repository and create a feature branch from `main`.
2. Add unit or integration tests covering your new feature or bug fix.
3. Ensure `cargo test` and `cargo clippy --all-targets -- -D warnings` pass with zero warnings.
4. Open a Pull Request with a clear summary of the changes.
