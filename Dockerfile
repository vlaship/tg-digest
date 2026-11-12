# Build and runtime are separate stages: the final image has only one binary.
#
# We do not use Alpine/musl on purpose: libsql compiles SQLite from C, and with
# musl that causes extra trouble. rustls is already a dependency, so the image
# does not need OpenSSL — ca-certificates is enough.
#
# The base images are pinned by digest, the same way Cargo.lock pins crates.
# rust-toolchain.toml selects the compiler (the same version as CI).
FROM rust:1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS build
WORKDIR /src

# Copy only the manifests first: the dependency layer survives changes in src/.
# Cargo.lock must be copied — it pins glass_pumpkin 2.0.0-rc0,
# without it the build fails inside grammers-crypto.
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# The version for `/status` and the start log: the build time (the same as the
# image tag) and the git commit. CI passes both; a local build without them shows
# "dev". They are after the dependency layer, so a new build does not rebuild the
# dependencies.
ARG BUILD_TIME=""
ARG GIT_SHA=""
# The file time must change, or cargo reuses the stub.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && echo 'digest:x:1000:1000::/data:/bin/sh' >> /etc/passwd \
    && echo 'digest:x:1000:' >> /etc/group \
    && mkdir -p /data \
    && chown 1000:1000 /data

# All working files are in the volume: config.toml, tg.session, digest.db.
VOLUME /data
WORKDIR /data

COPY --from=build /src/target/release/tg-digest /usr/local/bin/tg-digest
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
# The entrypoint starts as root only to give /data to user digest (uid 1000).
# A mounted volume keeps the host owner. The process then runs as digest.
RUN chmod 755 /usr/local/bin/docker-entrypoint.sh

# Docker collects the logs, so we write to stdout without buffering.
ENV RUST_LOG=tg_digest=info,warn
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
