#! /bin/bash

docker-compose exec metadata redis-cli set secret_key::ANOTREAL notrealrnrELgWzOk3IfjzDKtFBhDby
# S3_PROXY__REDIS__URL=redis://127.0.0.1:6379 S3_PROXY__OPENDAL_PROVIDER=redis S3_PROXY__OPENDAL__ROOT='/tmp' S3_PROXY__OPENDAL__ENDPOINT='tcp://127.0.0.1:6379' cargo run
S3_PROXY__REDIS__URL=redis://127.0.0.1:6379 S3_PROXY__OPENDAL_PROVIDER=memory S3_PROXY__OPENDAL__ROOT='/tmp' cargo run
