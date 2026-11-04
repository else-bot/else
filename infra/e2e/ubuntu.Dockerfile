# The source is bind-mounted at runtime, so edits never invalidate the SDK or
# distribution prerequisites. Only the new native arm64 lane uses this image.
ARG RUST_IMAGE=rust:1
FROM ${RUST_IMAGE} AS rust
FROM ubuntu:26.04

RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    sudo python3 build-essential pkg-config curl ca-certificates git jq tar gzip \
    docker-cli docker-compose-v2 file \
    postgresql-client-18 libssl-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libxcb1-dev libx11-xcb-dev libx11-dev libfontconfig-dev \
    libfreetype-dev libvulkan1 libzstd-dev \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -s /bin/bash tester \
    && echo 'tester ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/tester \
    && mkdir -p /cache/build /cache/microsandbox \
    && chown -R tester:tester /cache

COPY --from=rust --chown=tester:tester /usr/local/cargo /cache/toolchain/cargo
COPY --from=rust --chown=tester:tester /usr/local/rustup /cache/toolchain/rustup
ENV CARGO_HOME=/cache/toolchain/cargo RUSTUP_HOME=/cache/toolchain/rustup
ENV CARGO_TARGET_DIR=/cache/build
ENV PATH=/cache/toolchain/cargo/bin:/usr/lib/postgresql/18/bin:$PATH
USER tester
WORKDIR /src
