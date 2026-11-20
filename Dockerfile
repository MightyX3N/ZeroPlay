# syntax=docker/dockerfile:1
# ZeroPlay server with the playit.gg agent built in.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p zeroplay-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# playit.gg agent (playitd 2.0.2, BSD-2-Clause), pinned with checksums.
ARG TARGETARCH
ADD --checksum=sha256:9d27a8dbb4d2d8a7107fcabfa33028c789a30a64c97a36447fe4afb94e08dde9 https://github.com/playit-cloud/playit-agent/releases/download/v2.0.2/playit-linux-amd64 /tmp/playit-amd64
ADD --checksum=sha256:aed86fc2b82a772ca0e1d5cddec6c3123f96e12a099f7ae45aaa59a9a430a2cd https://github.com/playit-cloud/playit-agent/releases/download/v2.0.2/playit-linux-aarch64 /tmp/playit-arm64
RUN cp "/tmp/playit-${TARGETARCH:-amd64}" /usr/local/bin/playit && chmod 755 /usr/local/bin/playit && rm -f /tmp/playit-*

COPY --from=build /src/target/release/zeroplay-server /usr/local/bin/zeroplay-server
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

ENV ZEROPLAY_DATA=/data \
    ZEROPLAY_HTTP=0.0.0.0:8080 \
    ZEROPLAY_RTC_PORTS=50000-50009
VOLUME /data
EXPOSE 8080/tcp 50000-50009/udp
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
