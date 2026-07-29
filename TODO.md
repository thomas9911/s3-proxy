# TODO

## Priority

- [x] Implement public/private access control for buckets and objects.
- [x] Add regression coverage for malformed `DeleteObjects` XML.
- [x] Add regression coverage for more than 1,000 delete keys.
- [x] Add regression coverage for invalid `Content-MD5` values.
- [x] Add regression coverage for `DeleteObjects` quiet mode.
- [x] Add regression coverage for per-object delete failures.

## S3 API

- [x] Implement bucket versioning, object version IDs, delete markers, and `ListObjectVersions`.
- [x] Introduce principals/users so multiple access keys can share one storage namespace.
- [x] Implement `ListAccessKeys` without returning secret keys.
- [x] Implement `UpdateAccessKey` to activate or deactivate credentials for safe key rotation.
- [x] Track and expose access-key last-used information.
- [x] Apply principal-based ownership and authorization across buckets and objects.
- [x] Add an administrative API for creating access keys.
- [x] Implement bucket policy storage and evaluation.
- [x] Implement `PutBucketPolicy`, `GetBucketPolicy`, and `DeleteBucketPolicy`.
- [x] Support policy actions for anonymous and access-key requests, including read, write, delete, and list.
- [x] Map the simplified `public-read` ACL behavior to bucket-policy rules.
- [x] Implement multipart upload APIs.
- [x] Implement `CopyObject`.
- [x] Implement ranged GET requests.
- [x] Audit and expand S3 compatibility error responses.

## Reliability

- [x] Make object deletion and metadata deletion consistent when one side fails.
- [x] Add cleanup and retry handling for partial operations.
- [x] Add metadata schema migrations and indexes where needed.

## Performance

- [x] Add OpenDAL operation timing instrumentation.
- [x] Add metadata-store timing instrumentation.
- [x] Benchmark FS, Redis, SQLite, PostgreSQL, and remote OpenDAL services consistently.

## Operations

- [x] Add per-principal storage and request quotas.
- [x] Add structured request and audit logging.
- [x] Add health and readiness endpoints.
- [x] Add metrics for HTTP, storage, and metadata operations.
- [x] Build a management dashboard backed by the administrative API with separate admin authentication.
- [x] Create buckets from the management dashboard.
- [x] Upload files from the management dashboard.
- [x] List files in the management dashboard.
- [x] Generate presigned URLs in the management dashboard.
- [x] Delete buckets and objects from the management dashboard with confirmation.
- [x] View object metadata, versions, public state, and bucket policies in the management dashboard.
- [x] Download objects and generate presigned POST forms from the management dashboard.
- [x] Create, disable, rotate, and delete access keys from the management dashboard.
- [x] View multipart uploads and abort stale uploads from the management dashboard.
- [x] View and configure per-principal quota usage from the management dashboard.
- [x] Search and filter buckets and objects by prefix in the management dashboard.
- [x] View request/audit logs and metrics dashboards in the management dashboard.
- [x] View and edit bucket versioning and bucket policies in the management dashboard.
- [x] Show metadata-store and OpenDAL backend status in the management dashboard.
