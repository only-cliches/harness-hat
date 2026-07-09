# harness-hat .NET / C# image
#
# Build after harness-hat-base:local (from repo root):
#   docker build -t harness-hat-dotnet:local -f docker/dotnet.dockerfile docker/

FROM harness-hat-base:local

USER root

ENV DOTNET_CLI_TELEMETRY_OPTOUT=1
ENV DOTNET_NOLOGO=1
ENV DOTNET_SKIP_FIRST_TIME_EXPERIENCE=1

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      pkg-config \
      jq \
      shellcheck \
      direnv \
      sqlite3 \
      libsqlite3-dev \
      libssl-dev \
      dotnet-sdk-8.0 \
      dotnet-sdk-10.0; \
    rm -rf /var/lib/apt/lists/*; \
    dotnet --info

RUN set -eu; \
    mkdir -p /home/coder/.dotnet/tools; \
    chown -R coder:coder /home/coder/.dotnet

USER coder

ENV PATH="/home/coder/.dotnet/tools:/home/coder/.local/bin:${PATH}"

RUN set -eu; \
    dotnet tool install --global dotnet-ef; \
    dotnet tool install --global dotnet-format; \
    dotnet tool install --global csharpier

CMD ["bash"]
