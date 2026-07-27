



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

Configuration is read from environment variables with the `S3_PROXY__` prefix.
Double underscores separate nested fields.

| Variable | Description | Default |
| --- | --- | --- |
| `S3_PROXY__SERVER_HOST` | Address and port for the HTTP server | `0.0.0.0:3000` |
| `S3_PROXY__EXTERNAL_SERVER_HOST` | Public URL used when generating URLs | `http://0.0.0.0:3000` |
| `S3_PROXY__METADATA_BACKEND` | Metadata backend: `redis`, `sqlite`, or `postgres` | `sqlite` |
| `S3_PROXY__REDIS__URL` | Redis connection URL when using Redis metadata | Required for Redis |
| `S3_PROXY__SQLITE__URL` | SQLite URL when using SQLite metadata | `sqlite::memory:` |
| `S3_PROXY__POSTGRES__URL` | PostgreSQL URL when using PostgreSQL metadata | Required for PostgreSQL |
| `S3_PROXY__ADMIN__ACCESS_KEY` | Initial S3 access key to seed | Optional |
| `S3_PROXY__ADMIN__SECRET_KEY` | Initial S3 secret key to seed | Optional |
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
