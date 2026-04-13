FROM rust:1.83-bookworm AS builder
WORKDIR /app

COPY . .
RUN cargo build --release -p simgitd

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/simgitd /usr/local/bin/simgitd
WORKDIR /workspace

ENTRYPOINT ["simgitd"]
