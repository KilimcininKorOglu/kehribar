# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock clippy.toml ./
COPY migrations ./migrations
COPY locales ./locales
COPY static ./static
COPY templates ./templates
COPY src ./src
# Cargo notices a changed file by its modification time. COPY keeps the
# time of the build context, which can be older than the cached build, so
# an edited locale, template or source file would not be rebuilt. Touching
# the crate roots makes Cargo build this crate again; dependencies stay
# cached.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    touch src/lib.rs src/main.rs \
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
