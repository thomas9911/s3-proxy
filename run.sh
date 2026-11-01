#!/usr/bin/env bash
set -euo pipefail

# Local development defaults. Any of these can be overridden by the caller.
export S3_PROXY__SERVER_HOST="${S3_PROXY__SERVER_HOST:-127.0.0.1:3000}"
export S3_PROXY__EXTERNAL_SERVER_HOST="${S3_PROXY__EXTERNAL_SERVER_HOST:-http://127.0.0.1:3000}"
export S3_PROXY__METADATA_BACKEND="${S3_PROXY__METADATA_BACKEND:-sqlite}"
export S3_PROXY__SQLITE__URL="${S3_PROXY__SQLITE__URL:-sqlite::memory:}"
export S3_PROXY__OPENDAL_PROVIDER="${S3_PROXY__OPENDAL_PROVIDER:-memory}"
export S3_PROXY__ADMIN__ACCESS_KEY="${S3_PROXY__ADMIN__ACCESS_KEY:-admin}"
export S3_PROXY__ADMIN__SECRET_KEY="${S3_PROXY__ADMIN__SECRET_KEY:-admin-secret-key-change-me}"
export S3_PROXY__MANAGEMENT__USERNAME="${S3_PROXY__MANAGEMENT__USERNAME:-admin}"
export S3_PROXY__MANAGEMENT__PASSWORD="${S3_PROXY__MANAGEMENT__PASSWORD:-management-password-change-me}"

exec cargo run --release -- "$@"
