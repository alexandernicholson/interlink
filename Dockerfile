# Build stage
FROM rust:1.88-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Declared in the manifest ([[bench]]/[[example]] paths) — cargo needs them to parse.
COPY benches ./benches
COPY examples ./examples
RUN cargo build --release --bin interlinkd

# Runtime stage
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/interlinkd /interlinkd
EXPOSE 4143 4140 4192
USER nonroot:nonroot
ENTRYPOINT ["/interlinkd"]
