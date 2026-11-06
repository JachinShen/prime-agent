# Match the upstream release's Ubuntu 22.04 / GLIBC 2.35 baseline.
FROM ubuntu:22.04@sha256:b8b6ee6aa931ecd9d0d952abc34dc0e5f7c6a30c6bb71b079fe399fde0329c02

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential binutils ca-certificates curl git pkg-config python3 xz-utils \
    && rm -rf /var/lib/apt/lists/*

ENV CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup \
    PATH=/opt/cargo/bin:$PATH

# Use the same compiler version as the upstream release workflow.
RUN curl --fail --silent --show-error --location https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.98.1 \
    && rm /tmp/rustup.sh
