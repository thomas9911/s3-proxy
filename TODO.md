# TODO

## Priority

- [x] Implement public/private access control for buckets and objects.
- [ ] Add regression coverage for malformed `DeleteObjects` XML.
- [ ] Add regression coverage for more than 1,000 delete keys.
- [ ] Add regression coverage for invalid `Content-MD5` values.
- [ ] Add regression coverage for `DeleteObjects` quiet mode.
- [ ] Add regression coverage for per-object delete failures.

## S3 API

- [ ] Implement bucket policy storage and evaluation.
- [ ] Implement `PutBucketPolicy`, `GetBucketPolicy`, and `DeleteBucketPolicy`.
- [ ] Support policy actions for anonymous and access-key requests, including read, write, delete, and list.
- [ ] Map the simplified `public-read` ACL behavior to bucket-policy rules.
- [ ] Implement multipart upload APIs.
- [ ] Implement `CopyObject`.
- [ ] Implement ranged GET requests.
- [ ] Audit and expand S3 compatibility error responses.

## Reliability

- [ ] Make object deletion and metadata deletion consistent when one side fails.
- [ ] Add cleanup and retry handling for partial operations.
- [ ] Add metadata schema migrations and indexes where needed.

## Performance

- [ ] Add OpenDAL operation timing instrumentation.
- [ ] Add metadata-store timing instrumentation.
- [ ] Benchmark FS, Redis, SQLite, PostgreSQL, and remote OpenDAL services consistently.
