# syntax=docker/dockerfile:1

FROM rust:1-slim-trixie AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock clippy.toml ./
COPY migrations ./migrations
COPY locales ./locales
COPY static ./static
COPY templates ./templates
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
    && cp target/release/git-archive /usr/local/bin/git-archive

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git git-lfs openssh-client tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin gitarchive \
    && mkdir -p /data \
    && chown gitarchive:gitarchive /data
COPY --from=builder /usr/local/bin/git-archive /usr/local/bin/git-archive
ENV GIT_ARCHIVE_DATA_DIR=/data
USER 10001
WORKDIR /data
EXPOSE 8600
ENTRYPOINT ["tini", "--", "git-archive"]
