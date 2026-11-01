# Handoff: Single-Bucket Storage Layout

## Goal

Implement a read-write `single_bucket` storage layout so an existing OpenDAL
root can be served as one virtual S3 bucket without moving its files.

Example target mapping:

```text
physical root: D:\storage
physical file: D:\storage\photos\cat.jpg
S3 request:    s3://storage/photos/cat.jpg
internal tenant: admin
```

The physical root reserves `.s3-proxy/` for proxy-owned multipart and
versioning state. It must never appear in object listings.

## Current Worktree State

This work is intentionally incomplete and currently does not compile.

Already added:

- `Config.storage_layout` with `namespaced` (default) and `single_bucket`.
- `Config.single_bucket` with `namespace` and `name`.
- Startup validation for `single_bucket` configuration.
- `src/storage.rs` helpers:
  - `bucket_prefix`
  - `object_path`
  - `is_single_bucket`
  - `internal_path`
- Partial path migration for bucket listing, object listing, direct object
  PUT/GET/HEAD/DELETE/COPY, fixed bucket lifecycle, and metadata sync.
- `single_bucket` bucket creation returns `BucketAlreadyOwnedByYou` for the
  configured bucket and deletion is rejected.

## Intentional Compile Break

`src/versioning.rs` was changed so these functions now require `&Config`:

```rust
bucket_versioning(operator, config, namespace, bucket)
set_bucket_versioning(operator, config, namespace, bucket, status)
```

Their physical versioning configuration path already uses:

```rust
storage::internal_path(config, namespace, bucket, ...)
```

All call sites and the remaining versioning functions still need migration.
Run this to see the current repair list:

```text
cargo check --all-targets
```

## Required Next Steps

1. Change all versioning public functions to accept `&Config`:
   - `delete_bucket_state`
   - `record_put`
   - `prepare_overwrite`
   - `record_delete_marker`
   - `version`
   - `list_versions`
2. Propagate `config` to every versioning caller in:
   - `src/api/buckets.rs`
   - `src/api/list.rs`
   - `src/api/objects.rs`
   - `src/api/post.rs`
   - `src/api/management.rs` when the `management` feature is enabled.
3. Replace every hard-coded versioning path with `storage::internal_path`.
   For `single_bucket`, state belongs under `.s3-proxy/`; for `namespaced`, it
   remains under `namespace/.s3-proxy/`.
4. Migrate multipart paths in `src/api/post.rs` from
   `namespace/.multipart/<upload-id>` to an internal mapped path such as
   `.s3-proxy/multipart/<upload-id>` for `single_bucket`. Keep it hidden from
   object listing and metadata sync.
5. Migrate remaining direct `namespace/bucket/key` paths in management and
   quota code through `storage::bucket_prefix` and `storage::object_path`.
6. Update `sync.rs` and list operations to skip all `.s3-proxy/` paths.
7. Document these settings in `README.md` and `.env.example`:

```text
S3_PROXY__STORAGE_LAYOUT=single_bucket
S3_PROXY__SINGLE_BUCKET__NAMESPACE=admin
S3_PROXY__SINGLE_BUCKET__NAME=storage
```

8. Add integration coverage using a memory or filesystem operator whose root
   contains `photos/cat.jpg`; verify it is exposed as
   `s3://storage/photos/cat.jpg`, supports PUT/GET/HEAD/DELETE/COPY, and hides
   `.s3-proxy/`.
9. Run:

```text
cargo fmt --all
cargo check --all-targets
cargo test --all-targets
cargo check --no-default-features --all-targets
cargo test --no-default-features --all-targets
bun run typecheck
bunx biome lint tests
```

Do not revert unrelated worktree changes, including the prior metadata-sync
and management-feature work.
