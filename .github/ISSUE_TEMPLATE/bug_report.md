---
name: Bug Report
about: Report a bug or unexpected behavior in RustQL CLI, SQL engine, or TUI
title: "[Bug]: "
labels: bug
---

## Describe the Bug
A clear and concise description of what happened.

## Reproduction Steps
1. Input JSON shape / sample payload:
   ```json
   [{"id": 1, "name": "example"}]
   ```
2. Command or SQL query executed:
   ```bash
   rustql -q "SELECT * FROM ..." sample.json
   ```
3. Observed output or error message:

## Expected Behavior
What you expected the query or TUI to produce.

## Environment
- **RustQL Version** (`rustql --version`):
- **OS & Architecture** (e.g. Linux x86_64, macOS arm64, Windows 11 x64):
- **Terminal Emulator** (if TUI-related):
