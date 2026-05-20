# =====================================================================
# Q-BTC AEROSPACE DOCKER IMAGE (MULTI-STAGE BUILD)
# TARGET: Linux amd64/arm64 Native Execution & Chaos Engineering Ready
# =====================================================================

# STAGE 1: The Forge (Builder) - UPGRADED TO LATEST RUST KERNEL
FROM rust:slim-bookworm as builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    build-essential \
    clang \
    libclang-dev \
    cmake \
    protobuf-compiler

WORKDIR /usr/src/quantum-btc

COPY . .

# [CRITICAL FIX]: Injecting the physical key required for Tokio Console-Subscriber!
ENV RUSTFLAGS="--cfg tokio_unstable"

RUN cargo build --release

# STAGE 2: The Battleground (Runtime)
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    iproute2 \
    iptables \
    curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/quantum-btc/target/release/quantum-btc /usr/local/bin/qbtc-node

WORKDIR /root/.qbtc
ENV QBTC_DATA_DIR=/root/.qbtc

EXPOSE 8000 8001

ENTRYPOINT ["qbtc-node", "--datadir", "/root/.qbtc", "--port", "8000"]