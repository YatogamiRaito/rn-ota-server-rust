FROM rust:1-bookworm AS builder

WORKDIR /app

# Cache dependencies first: build a stub binary against the manifests only.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
COPY migrations ./migrations

# Touch the real sources so cargo rebuilds them over the cached deps.
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/rn-ota-server-rust /usr/local/bin/rn-ota-server-rust

# Run as an unprivileged user; the server needs no write access to anything.
RUN useradd --system --create-home --uid 10001 ota
USER ota

ENV HOST=0.0.0.0
ENV PORT=3010
EXPOSE 3010

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PORT}/health" || exit 1

# ENTRYPOINT, not CMD: it lets `docker run <image> --version` append the flag
# instead of replacing the command with a binary that does not exist.
ENTRYPOINT ["rn-ota-server-rust"]
