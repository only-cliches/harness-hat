# harness-hat Rust image
#
# Build after harness-hat-base:local (from repo root):
#   docker build -t harness-hat-rust:local -f docker/rust.dockerfile docker/

FROM harness-hat-base:local

USER root

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      cmake \
      pkg-config \
      clang \
      lld \
      mold \
      gdb \
      lldb \
      protobuf-compiler \
      sqlite3 \
      libsqlite3-dev \
      libssl-dev \
      jq \
      shellcheck \
      direnv; \
    rm -rf /var/lib/apt/lists/*

# Rust installs as root into the shared RUSTUP_HOME/CARGO_HOME. The container
# runs as `coder` (uid 1000), so hand ownership of both trees to that user in
# this same layer — otherwise `cargo build` (registry/cache writes), `cargo
# install` (writes to cargo/bin), and `rustup` updates all fail on root-owned
# paths. Doing the chown here (not a later layer) avoids duplicating the large
# toolchain with new ownership. a+rX keeps it readable if run under another uid.
# Pinned versions (H5): rustup-init is downloaded as the pinned release binary
# and verified against the sha256 that rust-lang.org publishes next to it (no
# more pipe-to-sh of an unpinned installer); the toolchain itself and the cargo
# tools are pinned to exact versions. rustup verifies toolchain component
# signatures/hashes internally. Bump by editing the ARGs and rebuilding.
ARG RUST_TOOLCHAIN=1.97.0
ARG RUSTUP_VERSION=1.29.0
ARG RUSTUP_SHA256_X86_64=4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10
ARG RUSTUP_SHA256_AARCH64=9732d6c5e2a098d3521fca8145d826ae0aaa067ef2385ead08e6feac88fa5792
ARG TARGETARCH
RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
      amd64|x86_64) rust_target="x86_64-unknown-linux-gnu"; rustup_sha="${RUSTUP_SHA256_X86_64}" ;; \
      arm64|aarch64) rust_target="aarch64-unknown-linux-gnu"; rustup_sha="${RUSTUP_SHA256_AARCH64}" ;; \
      *) echo "unsupported Rust architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    curl --proto '=https' --tlsv1.2 -fsSL \
      -o /tmp/rustup-init \
      "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/${rust_target}/rustup-init"; \
    echo "${rustup_sha}  /tmp/rustup-init" | sha256sum -c -; \
    chmod 755 /tmp/rustup-init; \
    /tmp/rustup-init -y --profile default --default-toolchain "${RUST_TOOLCHAIN}"; \
    rm -f /tmp/rustup-init; \
    rustup component add rustfmt clippy rust-src rust-analyzer; \
    cargo install --locked \
      cargo-edit@0.13.11 \
      cargo-watch@8.5.3 \
      cargo-nextest@0.9.140 \
      cargo-audit@0.22.2 \
      cargo-deny@0.20.2; \
    chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"; \
    chown -R coder:coder "${RUSTUP_HOME}" "${CARGO_HOME}"

USER coder

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
