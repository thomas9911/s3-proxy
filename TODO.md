# TODO

## Priority

- [x] Implement public/private access control for buckets and objects.
- [x] Add regression coverage for malformed `DeleteObjects` XML.
- [x] Add regression coverage for more than 1,000 delete keys.
- [x] Add regression coverage for invalid `Content-MD5` values.
- [x] Add regression coverage for `DeleteObjects` quiet mode.
- [x] Add regression coverage for per-object delete failures.

## S3 API

- [ ] Add an administrative API for creating access keys.
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
