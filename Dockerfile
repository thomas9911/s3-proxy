FROM chainguard/wolfi-base:latest AS builder

RUN apk add --no-cache build-base rust

ARG CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY templates ./templates
COPY src ./src

RUN cargo build --release --locked

FROM chainguard/wolfi-base:latest

RUN apk add --no-cache ca-certificates libgcc

COPY --from=builder /build/target/release/s3-proxy /usr/local/bin/s3-proxy

EXPOSE 3000
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/s3-proxy"]
