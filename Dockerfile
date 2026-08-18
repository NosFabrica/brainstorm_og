# ---- build (static musl) ----
# Pinned, not `rust:1-alpine`: a floating tag silently moves the toolchain under
# a lockfile-pinned build, which is the one thing `--locked` cannot protect.
FROM rust:1.93-alpine3.21 AS builder
RUN apk add --no-cache musl-dev build-base cmake make perl ca-certificates

WORKDIR /app

# Cache dependencies independently of source changes.
COPY Cargo.toml Cargo.lock ./
# Both targets must exist or cargo refuses to read the manifest.
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Fonts and the wordmark are vendored in the repo (see assets/README.md), so
# there is no font package to install and no `find` that can silently match
# nothing. The wordmark is `include_str!`d, so assets/ must be present to build.
COPY assets ./assets
COPY src ./src
# Bust the stub's cached mtime so the real binary rebuilds.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

# ---- runtime (scratch: static binary + assets + CA bundle) ----
FROM scratch
COPY --from=builder /app/target/release/brainstorm-og /brainstorm-og
COPY --from=builder /app/assets /assets
# reqwest's rustls loads system CA certs; ship the Mozilla bundle so HTTPS
# (avatar fetch) works on scratch.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Numeric because scratch has no /etc/passwd to resolve a name against. Nothing
# is written at runtime, so this costs nothing.
USER 65532:65532

ENV ASSETS_DIR=/assets \
    BIND_ADDR=0.0.0.0:8080 \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 8080
# No HEALTHCHECK: scratch has no shell to run one. /healthz reports font state
# and is wired to the k8s readiness/liveness probes instead.
ENTRYPOINT ["/brainstorm-og"]
