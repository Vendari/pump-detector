# Build from the repo root: docker build -f rust/Dockerfile rust
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked && cp target/release/pump-detector /pump-detector

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=build /pump-detector /usr/local/bin/pump-detector
EXPOSE 8000
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD curl -fsS http://127.0.0.1:8000/health || exit 1
CMD ["pump-detector"]
