#! /bin/bash

docker-compose exec metadata redis-cli set secret_key::ANOTREAL notrealrnrELgWzOk3IfjzDKtFBhDby
S3_PROXY__REDIS__URL=redis://127.0.0.1:6379 cargo run
