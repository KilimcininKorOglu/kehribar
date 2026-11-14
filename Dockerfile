# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS builder
WORKDIR /src
# The dependencies build in their own layer, from the manifests and empty
# crate roots, so a source change does not rebuild them. Plain layers work
# with the classic builder too, which has no cache mounts.
COPY Cargo.toml Cargo.lock clippy.toml ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY migrations ./migrations
COPY locales ./locales
COPY static ./static
COPY templates ./templates
COPY src ./src
# COPY keeps the file times of the build context, which can be older than
# the empty crate roots built above; touching them makes Cargo build this
# crate again.
RUN touch src/lib.rs src/main.rs \
    && cargo build --release --locked \
    && cp target/release/kehribar /usr/local/bin/kehribar

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git git-lfs openssh-client tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin kehribar \
    && mkdir -p /data \
    && chown kehribar:kehribar /data
COPY --from=builder /usr/local/bin/kehribar /usr/local/bin/kehribar
ENV KEHRIBAR_DATA_DIR=/data
USER 10001
WORKDIR /data
EXPOSE 8600
ENTRYPOINT ["tini", "--", "kehribar"]
