# ZeroPlay server with the playit.gg agent built in.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p zeroplay-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# playit.gg agent. Set PLAYIT_VERSION (for example 0.15.26) to pin a release.
ARG TARGETARCH
ARG PLAYIT_VERSION=latest
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in arm64) arch=aarch64 ;; *) arch=amd64 ;; esac; \
    if [ "$PLAYIT_VERSION" = latest ]; then \
      url="https://github.com/playit-cloud/playit-agent/releases/latest/download/playit-linux-$arch"; \
    else \
      url="https://github.com/playit-cloud/playit-agent/releases/download/v$PLAYIT_VERSION/playit-linux-$arch"; \
    fi; \
    curl -fsSL "$url" -o /usr/local/bin/playit; \
    chmod +x /usr/local/bin/playit

COPY --from=build /src/target/release/zeroplay-server /usr/local/bin/zeroplay-server
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV ZEROPLAY_DATA=/data \
    ZEROPLAY_HTTP=0.0.0.0:8080 \
    ZEROPLAY_RTC_PORTS=50000-50009
VOLUME /data
EXPOSE 8080/tcp 50000-50009/udp
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
