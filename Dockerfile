# ---- builder ----
FROM rust:1-bookworm AS builder
# opus crate は system libopus にリンクする。
RUN apt-get update && apt-get install -y --no-install-recommends \
        libopus-dev pkg-config cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
# 必要に応じて --features lavalink-discord-voice/dave を付与（DAVE/E2EE 有効化）。
RUN cargo build --release --bin lavalink-rs

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        libopus0 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/lavalink-rs /usr/local/bin/lavalink-rs
# application.yml は実行時にマウント/同梱する。
EXPOSE 2333
ENTRYPOINT ["lavalink-rs"]
