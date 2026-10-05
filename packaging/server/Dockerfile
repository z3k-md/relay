# Relay home server: a headless, always-on Relay device (docs/proposals/home-server.md).
#
#   docker build -f packaging/server/Dockerfile -t relay-server .
#
# Build context is the repository root.
FROM rust:1.98-bookworm AS build
WORKDIR /src
COPY . .
# The image already carries the compiler rust-toolchain.toml pins; without
# this, rustup downloads it again with rustfmt and clippy.
RUN rm rust-toolchain.toml
ARG RELAY_GIT_REV=unknown
ENV RELAY_GIT_REV=${RELAY_GIT_REV}
RUN cargo build --release --locked -p relay-cli \
    && install -m 0755 target/release/relay /usr/local/bin/relay

FROM debian:bookworm-slim
# No extra packages: peers are pinned by key, so there is no CA bundle to
# trust, and `relay run` stops cleanly on SIGTERM as PID 1.
RUN useradd --system --uid 10001 --home-dir /var/lib/relay relay \
    && mkdir -p /var/lib/relay \
    && chown relay:relay /var/lib/relay
COPY --from=build /usr/local/bin/relay /usr/local/bin/relay
COPY packaging/server/entrypoint.sh /usr/local/bin/relay-server
USER relay
ENV RELAY_HOME=/var/lib/relay/home \
    RELAY_DATA=/var/lib/relay/data \
    RELAY_NAME=relay-server \
    RELAY_LISTEN=0.0.0.0:47321
VOLUME ["/var/lib/relay"]
EXPOSE 47321/udp
ENTRYPOINT ["/usr/local/bin/relay-server"]
