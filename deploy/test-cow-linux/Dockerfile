FROM rust:latest

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        git \
        build-essential \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY . .

# Build the daemon + CLI, then run the overlayfs integration script.
CMD ["bash", "deploy/test-cow-linux/run.sh"]
