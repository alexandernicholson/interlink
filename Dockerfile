# Build stage
FROM rust:1.80-slim-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --bin interlinkd

# Runtime stage
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/interlinkd /interlinkd
EXPOSE 4143 4140 4192
USER nonroot:nonroot
ENTRYPOINT ["/interlinkd"]
