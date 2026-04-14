FROM rust:latest

WORKDIR /app

# Install FUSE3 and other dependencies needed for testing
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        fuse3 \
        libfuse3-dev \
        pkg-config \
        git \
        build-essential \
    && rm -rf /var/lib/apt/lists/*

# Copy the entire workspace
COPY . .

# Ensure FUSE device is available and run the Linux FUSE integration tests
CMD ["sh", "-c", "modprobe fuse 2>/dev/null || true && cargo test -p simgitd linux_integration_tests -- --ignored --nocapture --test-threads=1"]
