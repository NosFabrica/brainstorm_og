# ---- build (static musl) ----
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev build-base cmake make perl ca-certificates

WORKDIR /app

# Cache dependencies independently of source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
# Bust the stub's cached mtime so the real binary rebuilds.
RUN touch src/main.rs && cargo build --release

# Bundle a font for resvg (scratch has none). The alpine package installs as
# font-dejavu under /usr/share/fonts/dejavu; copy by name so the path can move.
RUN apk add --no-cache font-dejavu \
    && mkdir -p /assets \
    && find /usr/share/fonts -name 'DejaVuSans*.ttf' -exec cp {} /assets/ \;

# ---- runtime (scratch: static binary + font + CA bundle) ----
FROM scratch
COPY --from=builder /app/target/release/brainstorm-og /brainstorm-og
COPY --from=builder /assets /assets
# reqwest's rustls loads system CA certs; ship the Mozilla bundle so HTTPS
# (overview API + avatar fetch) works on scratch.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ENV ASSETS_DIR=/assets \
    BIND_ADDR=0.0.0.0:8080 \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 8080
ENTRYPOINT ["/brainstorm-og"]
