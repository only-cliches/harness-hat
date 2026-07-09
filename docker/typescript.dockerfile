# harness-hat TypeScript / Node / Bun image
#
# Build after harness-hat-base:local (from repo root):
#   docker build -t harness-hat-typescript:local -f docker/typescript.dockerfile docker/

FROM harness-hat-base:local

USER root

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      python3 \
      python3-pip \
      pkg-config \
      jq \
      shellcheck \
      direnv; \
    rm -rf /var/lib/apt/lists/*

# Pinned versions (H5): npm packages are integrity-checked against registry
# metadata; Bun is downloaded as a release zip and verified against the sha256
# published in the release's SHASUMS256.txt (no more pipe-to-bash of an
# unpinned installer). Bump by editing the ARGs (and Bun's hashes) and rebuilding.
ARG TYPESCRIPT_VERSION=7.0.2
ARG TSX_VERSION=4.23.0
ARG VITE_VERSION=8.1.4
ARG ESLINT_VERSION=10.6.0
ARG PRETTIER_VERSION=3.9.5
ARG NODEMON_VERSION=3.1.14
RUN set -eu; \
    corepack enable; \
    npm install -g \
      "typescript@${TYPESCRIPT_VERSION}" \
      "tsx@${TSX_VERSION}" \
      "vite@${VITE_VERSION}" \
      "eslint@${ESLINT_VERSION}" \
      "prettier@${PRETTIER_VERSION}" \
      "nodemon@${NODEMON_VERSION}"

ARG BUN_VERSION=1.3.14
ARG BUN_SHA256_X64=951ee2aee855f08595aeec6225226a298d3fea83a3dcd6465c09cbccdf7e848f
ARG BUN_SHA256_X64_BASELINE=a063908ae08b7852ca10939bbdc6ceed3ddabce8fb9402dce83d65d73b36e6c7
ARG BUN_SHA256_AARCH64=a27ffb63a8310375836e0d6f668ae17fa8d8d18b88c37c821c65331973a19a3b
ARG TARGETARCH
# The baseline build covers x64 CPUs without AVX2, matching what Bun's install
# script would have auto-selected. Detection runs on the build host, so build
# on hardware representative of where the image will run.
RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
      amd64|x86_64) \
        if grep -q avx2 /proc/cpuinfo 2>/dev/null; then \
          bun_pkg="bun-linux-x64"; bun_sha="${BUN_SHA256_X64}"; \
        else \
          bun_pkg="bun-linux-x64-baseline"; bun_sha="${BUN_SHA256_X64_BASELINE}"; \
        fi ;; \
      arm64|aarch64) bun_pkg="bun-linux-aarch64"; bun_sha="${BUN_SHA256_AARCH64}" ;; \
      *) echo "unsupported Bun architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/bun.zip \
      "https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}/${bun_pkg}.zip"; \
    echo "${bun_sha}  /tmp/bun.zip" | sha256sum -c -; \
    unzip -oq /tmp/bun.zip -d /tmp/bun-extract; \
    install -d "${BUN_INSTALL}/bin"; \
    install -m 0755 "/tmp/bun-extract/${bun_pkg}/bun" "${BUN_INSTALL}/bin/bun"; \
    ln -sf bun "${BUN_INSTALL}/bin/bunx"; \
    rm -rf /tmp/bun.zip /tmp/bun-extract; \
    "${BUN_INSTALL}/bin/bun" --version

USER coder

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
