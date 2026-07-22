# Build stage
FROM rust:1.88-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Declared in the manifest ([[bench]]/[[example]] paths) — cargo needs them to parse.
COPY benches ./benches
COPY examples ./examples
RUN cargo build --release --bin interlinkd

# Runtime stage. The same image is used by the short-lived Kubernetes
# redirect initializer, so it contains iptables and a shell in addition to
# the non-root proxy runtime.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    iptables \
    && groupadd --gid 1337 interlink \
    && useradd --uid 1337 --gid 1337 --no-create-home --shell /usr/sbin/nologin interlink \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/interlinkd /usr/local/bin/interlinkd
EXPOSE 5433 15001 15000 4192
USER 1337:1337
ENTRYPOINT ["/usr/local/bin/interlinkd"]
