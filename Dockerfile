FROM rust:1.99.1-bookworm AS build

RUN apt-get update \
    && apt-get install --no-install-recommends -y cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY web ./web
RUN cargo build --locked --release --bin closet --bin ha-closet

FROM debian:bookworm-slim

ARG BUILD_VERSION=dev
LABEL org.opencontainers.image.title="Closet" \
      org.opencontainers.image.source="https://github.com/jessfraz/ha-closet" \
      org.opencontainers.image.licenses="MIT" \
      io.hass.name="Closet" \
      io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="aarch64|amd64"

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates gosu \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 closet \
    && useradd --uid 10001 --gid closet --no-create-home --shell /usr/sbin/nologin closet \
    && install -d -o closet -g closet /data

COPY --from=build /src/target/release/closet /usr/local/bin/closet
COPY --from=build /src/target/release/ha-closet /usr/local/bin/ha-closet
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV CLOSET_BIND=0.0.0.0:8080 \
    CLOSET_DATABASE=/data/closet.sqlite3

WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
