# S3 Proxy Agent Guide

## Project Overview

This repository implements an S3-compatible HTTP proxy in Rust. Axum handles the HTTP routes, OpenDAL provides the object-storage service, and a separate metadata store tracks credentials, bucket/object visibility, policies, and object metadata.

The supported metadata backends are Redis, SQLite, and PostgreSQL. OpenDAL providers are selected at runtime through `S3_PROXY__OPENDAL_PROVIDER` and configured through `S3_PROXY__OPENDAL__*` environment variables.

## Repository Layout

- `src/main.rs`: configuration, metadata-store construction, OpenDAL operator construction, and server startup.
- `src/api.rs`: route wiring and shared API helpers.
- `src/api/buckets.rs`: bucket operations and bucket policies.
- `src/api/objects.rs`: object CRUD, copy, ranges, ACL handling, and object metadata responses.
- `src/api/post.rs`: multipart uploads, DeleteObjects, and presigned POST handling.
- `src/api/list.rs`: bucket and object listing.
- `src/metadata.rs`: `MetadataStore` trait and shared metadata types.
- `src/metadata/sql.rs`: SQLite/PostgreSQL metadata implementation.
- `src/metadata/redis.rs`: Redis metadata implementation.
- `src/backends.rs`: OpenDAL service probing for `--backends`.
- `tests/s3-regression.test.ts`: AWS SDK and rclone compatibility contract.
- `tests/run-regression.ts`: wrapper that runs the contract against MinIO and proxy configurations.
- `tests/s3_proxy.rs`: Rust integration test.
- `TODO.md`: current feature and compatibility roadmap.

## Development Commands

Run these before submitting a change:

```text
cargo fmt --all
cargo check --all-targets
cargo test --all-targets
bun run typecheck
bunx biome lint tests
```

Run the complete S3 compatibility matrix:

```text
bun run regression
```

The regression wrapper starts MinIO in Docker when `S3_TEST_EXTERNAL_ENDPOINT` and `S3_TEST_ENDPOINT` are not set. It then tests the proxy with Redis, SQLite, FS, Sled, and PostgreSQL configurations. Docker Desktop must be running. The tests also expect the repository-local `rclone.exe`, unless `RCLONE` is set.

Run only the TypeScript contract against an already-running endpoint:

```text
$env:S3_TEST_TARGET = "external"
$env:S3_TEST_ENDPOINT = "http://127.0.0.1:9000"
bun test tests/s3-regression.test.ts
```

## Configuration

Configuration uses the `S3_PROXY__` prefix with `__` as the nested-field separator. Typical local proxy settings are:

```text
S3_PROXY__METADATA_BACKEND=sqlite
S3_PROXY__SQLITE__URL=sqlite://target/s3-proxy.db
S3_PROXY__OPENDAL_PROVIDER=memory
S3_PROXY__OPENDAL__ROOT=/tmp
```

For Redis, use `S3_PROXY__METADATA_BACKEND=redis` and `S3_PROXY__REDIS__URL`. For PostgreSQL, use `S3_PROXY__METADATA_BACKEND=postgres` and `S3_PROXY__POSTGRES__URL`.

## Implementation Guidance

- Preserve S3 semantics for overwrite operations: clear old object metadata and reset public visibility before applying replacement metadata or an explicit ACL.
- Keep object data in OpenDAL and access-control/metadata state in `MetadataStore`; do not couple API handlers directly to a specific metadata backend.
- Use the `MetadataStore` trait for new metadata operations and implement them in every configured backend.
- Multipart uploads must validate the durable manifest, part numbers, part ETags, and missing parts. Completion should stream parts into the destination rather than assembling the entire object in memory.
- Presigned POST policies must validate expiration and every supported condition. Do not silently ignore unknown or malformed conditions.
- Return S3-shaped error responses through the existing helpers rather than ad hoc response bodies.
- Keep provider-specific behavior isolated from API logic. Add or remove OpenDAL service features in `Cargo.toml` consistently with the probing code.

## Change Safety

- Do not commit generated databases, `target/` output, Docker state, or `rclone.exe` unless explicitly requested.
- Do not reset or discard unrelated working-tree changes.
- Add regression coverage for externally visible S3 behavior, especially access-control, overwrite, multipart, policy, pagination, and error cases.
- Run the full regression matrix for changes affecting routing, metadata, OpenDAL operations, multipart uploads, or S3 compatibility.
