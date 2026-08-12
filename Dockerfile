# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && cp target/release/claude-code-proxy /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates socat curl iproute2 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/bin/claude-code-proxy /usr/local/bin/claude-code-proxy
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV CCP_CONFIG_DIR=/config \
    CCP_BIND_ADDRESS=0.0.0.0 \
    PORT=18765

EXPOSE 18765 1455
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
