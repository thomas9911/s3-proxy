# Serving Existing Files

S3 Proxy stores data separately from its metadata database. Before serving an
existing OpenDAL backend, its physical path layout must be mapped to the S3
bucket and object-key model.

## Current Layout

The default layout is namespaced:

```text
physical path: namespace/bucket/object-key
S3 request:    s3://bucket/object-key
```

The namespace is an internal tenant/principal boundary derived from the access
key. It is not part of a standard S3 URL.

For example:

```text
admin/photos/cat.jpg -> s3://photos/cat.jpg
```

The `--sync-metadata` command currently imports this layout. Run a dry run
first, then import the storage-derived metadata:

```text
cargo run --release -- --sync-metadata --dry-run
cargo run --release -- --sync-metadata --namespace admin
```

The import records namespace ownership and object size, content type, ETag,
and last-modified values. It never changes object data, access keys, ACLs,
bucket policies, or versioning state.

## Planned Single-Bucket Layout

The first flat-layout mode will expose one existing directory or OpenDAL root
as one configured virtual S3 bucket.

```text
physical root: D:\storage
physical file: D:\storage\photos\cat.jpg
S3 request:    s3://storage/photos/cat.jpg
```

The proxy will use a configured internal namespace and virtual bucket name.
The initial configuration shape will be:

```text
S3_PROXY__STORAGE_LAYOUT=single_bucket
S3_PROXY__SINGLE_BUCKET__NAMESPACE=admin
S3_PROXY__SINGLE_BUCKET__NAME=storage
S3_PROXY__OPENDAL_PROVIDER=fs
S3_PROXY__OPENDAL__ROOT=D:\storage
```

This mode must route object operations, listings, multipart state, versioning,
quotas, and metadata synchronization through a shared storage-path mapper. It
must not move or copy existing object data.

## Future Directory-Buckets Layout

A later `directory_buckets` layout will expose each immediate child directory
of an OpenDAL root as an S3 bucket:

```text
physical root: D:\storage
physical file: D:\storage\photos\cat.jpg
S3 request:    s3://photos/cat.jpg
```

This will be added behind a `directory-buckets` Cargo feature and enabled with
`S3_PROXY__STORAGE_LAYOUT=directory_buckets`. The shared storage-path mapper
introduced for `single_bucket` will make this an additive layout rather than a
second path-model refactor.
