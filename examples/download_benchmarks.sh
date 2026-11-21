#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_FILE="${SCRIPT_DIR}/github_events.ndjson"
ARCHIVE_URL="${1:-https://data.gharchive.org/2024-01-01-15.json.gz}"

echo "Downloading and streaming ${ARCHIVE_URL} -> ${TARGET_FILE}..."
curl -fSL "${ARCHIVE_URL}" | gzip -dc > "${TARGET_FILE}"
echo "Saved $(wc -l < "${TARGET_FILE}") events ($(du -h "${TARGET_FILE}" | cut -f1)) to ${TARGET_FILE}"
