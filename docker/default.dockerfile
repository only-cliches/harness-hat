# harness-hat default image — TypeScript + headless browser
#
# Uses the shared Ubuntu base (`harness-hat-base:local`) so strict-network
# and proxy bootstrap behavior stays consistent with manager-launched images.
#
# Build after harness-hat-base:local (context: docker/):
#   docker build -t harness-hat-default:local -f docker/default.dockerfile docker/

# Bun's official manifest is pinned and selects the correct amd64 or arm64
# image without CPU-feature detection during the Docker build.
ARG BUN_IMAGE=oven/bun:1.3.14@sha256:e10577f0db68676a7024391c6e5cb4b879ebd17188ab750cf10024a6d700e5c4
FROM ${BUN_IMAGE} AS bun

FROM harness-hat-base:local

USER root

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:${PATH}"
ENV PLAYWRIGHT_BROWSERS_PATH=/usr/local/share/ms-playwright
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/local/bin/chromium

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
# metadata. Bun comes from its pinned official multi-architecture image; the
# Playwright browser is revision-pinned by its package version. Bump the image
# tag/digest and package versions together when updating this image.
ARG PNPM_VERSION=11.11.0
ARG TYPESCRIPT_VERSION=7.0.2
ARG TSX_VERSION=4.23.0
ARG PLAYWRIGHT_VERSION=1.62.1
ARG AGENT_BROWSER_VERSION=0.34.0
RUN set -eu; \
    npm install -g \
      "pnpm@${PNPM_VERSION}" \
      "typescript@${TYPESCRIPT_VERSION}" \
      "tsx@${TSX_VERSION}" \
      "playwright@${PLAYWRIGHT_VERSION}" \
      "agent-browser@${AGENT_BROWSER_VERSION}"

COPY --from=bun /usr/local/bin/bun ${BUN_INSTALL}/bin/bun
RUN ln -sf bun "${BUN_INSTALL}/bin/bunx" \
    && "${BUN_INSTALL}/bin/bun" --version

# Playwright supplies a Chromium binary and every shared library it needs.
# `agent-browser` uses the stable symlink below, so it does not download a
# second browser at runtime. Playwright's Linux ARM64 support makes this work
# on both common Docker host architectures.
RUN set -eu; \
    install -d -m 0755 "${PLAYWRIGHT_BROWSERS_PATH}"; \
    playwright install --with-deps chromium; \
    chromium_path="$(find "${PLAYWRIGHT_BROWSERS_PATH}" -type f \
      \( -path '*/chrome-linux/chrome' -o -path '*/chrome-linux64/chrome' \) -print -quit)"; \
    test -n "${chromium_path}"; \
    ln -sf "${chromium_path}" "${AGENT_BROWSER_EXECUTABLE_PATH}"; \
    chmod -R a+rX "${PLAYWRIGHT_BROWSERS_PATH}"; \
    chown -R coder:coder "${PLAYWRIGHT_BROWSERS_PATH}"; \
    test -x "${AGENT_BROWSER_EXECUTABLE_PATH}"; \
    playwright --version; \
    agent-browser --help >/dev/null

# Make the agent-browser workflow available as a Codex skill in every default
# session. The installed CLI carries the version-matched workflow; this small
# discovery stub avoids fetching a separate Git repository during image builds.
RUN set -eu; \
    skill_path=/home/coder/.codex/skills/agent-browser/SKILL.md; \
    install -D -o coder -g coder -m 0644 /dev/null "${skill_path}"; \
    printf '%s\n' \
      '---' \
      'name: agent-browser' \
      'description: Browser automation with the installed agent-browser CLI.' \
      '---' \
      '' \
      '# Agent Browser' \
      '' \
      'Use `agent-browser` for browser automation.' \
      'Before browser work, load the current workflow with:' \
      '' \
      '`agent-browser skills get core`' \
      > "${skill_path}"; \
    test -s "${skill_path}"

USER coder

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
