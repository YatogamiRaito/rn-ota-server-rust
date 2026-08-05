FROM rust:1-bookworm AS builder

WORKDIR /app

# Cache dependencies first: build a stub binary against the manifests only.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
COPY migrations ./migrations

# Touch the real sources so cargo rebuilds them over the cached deps.
RUN touch src/main.rs src/lib.rs && cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/rn-ota-server-rust /usr/local/bin/rn-ota-server-rust

ENV HOST=0.0.0.0
ENV PORT=3010
EXPOSE 3010

CMD ["rn-ota-server-rust"]
