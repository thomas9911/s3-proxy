



backends:
- gc: https://github.com/fullstorydev/emulators
- s3: https://min.io/
- azure: https://github.com/Azure/Azurite

## Temporarily unsupported backends

Azure Blob Storage and Google Cloud Storage are planned backends, but are not
currently enabled. Their OpenDAL service dependencies pull in `rsa`, which is
affected by the [Marvin timing attack advisory](https://rustsec.org/advisories/RUSTSEC-2023-0071)
and has no fixed upstream release. They will be re-enabled once a safe
dependency path is available.

## Environment variables

## Cargo features

The management dashboard is enabled by default through the `management` Cargo
feature. Build without it to exclude the `/admin` routes, management handlers,
and management Askama templates:

```text
cargo build --release --no-default-features
```

Configuration is read from environment variables with the `S3_PROXY__` prefix.
Double underscores separate nested fields.

| Variable | Description | Default |
| --- | --- | --- |
| `S3_PROXY__SERVER_HOST` | Address and port for the HTTP server | `0.0.0.0:3000` |
| `S3_PROXY__EXTERNAL_SERVER_HOST` | Public URL used when generating URLs | `http://0.0.0.0:3000` |
| `S3_PROXY__MAX_REQUEST_BODY_BYTES` | Maximum size of a single HTTP request body | `268435456` (256 MiB) |
| `S3_PROXY__METADATA_BACKEND` | Metadata backend: `redis`, `sqlite`, or `postgres` | `sqlite` |
| `S3_PROXY__REDIS__URL` | Redis connection URL when using Redis metadata | Required for Redis |
| `S3_PROXY__SQLITE__URL` | SQLite URL when using SQLite metadata | `sqlite::memory:` |
| `S3_PROXY__POSTGRES__URL` | PostgreSQL URL when using PostgreSQL metadata | Required for PostgreSQL |
| `S3_PROXY__ADMIN__ACCESS_KEY` | Initial S3 access key to seed | Optional |
| `S3_PROXY__ADMIN__SECRET_KEY` | Initial S3 secret key to seed | Optional |
| `S3_PROXY__MANAGEMENT__USERNAME` | HTTP Basic username for the management dashboard | Disabled unless both management variables are set |
| `S3_PROXY__MANAGEMENT__PASSWORD` | HTTP Basic password for the management dashboard | Disabled unless both management variables are set |
| `S3_PROXY__QUOTAS__MAX_STORAGE_BYTES` | Maximum retained storage per principal, including object versions | Unlimited |
| `S3_PROXY__QUOTAS__MAX_REQUESTS_PER_MINUTE` | Maximum authenticated requests per principal per minute | Unlimited |
| `S3_PROXY__OPENDAL_PROVIDER` | OpenDAL service name, such as `s3`, `fs`, `memory`, or `sled` | `memory` |
| `S3_PROXY__OPENDAL__*` | Options passed to the selected OpenDAL service | Service-dependent |

OpenDAL options use the provider's option names after `S3_PROXY__OPENDAL__`.
For example, an S3 service can be configured with
`S3_PROXY__OPENDAL__ENDPOINT`, `S3_PROXY__OPENDAL__BUCKET`,
`S3_PROXY__OPENDAL__ACCESS_KEY_ID`, and
`S3_PROXY__OPENDAL__SECRET_ACCESS_KEY`. A filesystem service commonly uses
`S3_PROXY__OPENDAL__ROOT`.

Set both admin variables to create the initial credential on startup. Existing
credentials are not overwritten.

## Metadata synchronization

To adopt existing data, arrange it in the proxy storage layout
`namespace/bucket/key`, configure OpenDAL and the metadata store, then run a
one-shot metadata import before starting the server:

```text
cargo run --release -- --sync-metadata --dry-run
cargo run --release -- --sync-metadata
```

Use `--namespace admin` to import only one namespace. The command creates a
default namespace owner only when it is absent and imports content type, size,
ETag, and last-modified values from OpenDAL. It never changes object data,
access keys, ACLs, bucket policies, or versioning state. Imported objects are
private unless access-control metadata already exists.

This command does not remap a flat external S3 bucket into the proxy layout.
For an existing filesystem or OpenDAL S3 service, set its root/prefix or move
the data so that the proxy can see `namespace/bucket/key` paths first.

## Single-bucket layout

To serve an existing OpenDAL root as one S3 bucket without moving files, set:

```text
S3_PROXY__STORAGE_LAYOUT=single_bucket
S3_PROXY__SINGLE_BUCKET__NAMESPACE=admin
S3_PROXY__SINGLE_BUCKET__NAME=storage
```

For example, `photos/cat.jpg` at the configured physical root is exposed as
`s3://storage/photos/cat.jpg`. Proxy-owned multipart and versioning state is
stored under `.s3-proxy/` and is excluded from object listings.

## Operations

## Build profiles

The default build includes the management UI, Redis and PostgreSQL metadata
backends, and every OpenDAL provider supported by this repository:

```text
cargo build --release
```

For a smaller local-only build with SQLite metadata and only the `memory` and
filesystem OpenDAL providers, use:

```text
cargo build --release --no-default-features
```

The minimal build intentionally rejects Redis, PostgreSQL, and other provider
names at configuration time because they are not compiled into the binary.

`GET /healthz` reports process health, `GET /readyz` verifies the metadata and
OpenDAL services, and `GET /metrics` exposes Prometheus text metrics. Requests
and authenticated principal activity are emitted as structured `tracing` audit
events.

Set both management variables to enable the separately authenticated dashboard
at `GET /admin`. Its JSON API is available below `/admin/api/` and lists
principals and sanitized access-key state; it never exposes secret keys.

metadata:
- sqlite (in-memory by default)
- redis
- postgres







## extra

maybe use https://github.com/seaweedfs/seaweedfs as s3 


## metadata

```txt                              
keypair ---|                 |--- dir -|- cors
           |                 |         |- acl
keypair ---|--- namespace ---|--- dir
           |                 |         |- cors
keypair ---|                 |--- dir -|- acl
```
