# Build and runtime are separate stages: the final image has only one binary.
#
# We do not use Alpine/musl on purpose: libsql compiles SQLite from C, and with
# musl that causes extra trouble. rustls is already a dependency, so the image
# does not need OpenSSL — ca-certificates is enough.
#
# The base images are pinned by digest, the same way Cargo.lock pins crates.
# The build image is Rust 1.99.1, the same version as rust-toolchain.toml and CI.
# That file is not copied in: rustup would download rustfmt and clippy for it,
# and the build needs neither. Change the tag, the digest and the file together.
FROM rust:1.99.1-bookworm@sha256:e6f2cff35d0f309acf8433b4f05878ed77092602c5065cc81801177da0bb3a9c AS build
WORKDIR /src

# Copy only the manifests first: the dependency layer survives changes in src/.
# Cargo.lock must be copied — it pins glass_pumpkin 2.0.0-rc0,
# without it the build fails inside grammers-crypto.
COPY Cargo.toml Cargo.lock ./
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

# Node 22 LTS for agent CLIs installed into the volume (not into this image).
# Bookworm apt still has Node 18 (EOL); CLIs need 20+.
FROM node:22-bookworm-slim@sha256:e9f565d32859f0cbfb52125eafe8be39390f24dac302dc3cdf566e8ba3ab21ee AS node

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && echo 'digest:x:1000:1000::/data:/bin/sh' >> /etc/passwd \
    && echo 'digest:x:1000:' >> /etc/group \
    && mkdir -p /data \
    && chown 1000:1000 /data

# Node + npm from the official image (not apt). CLIs themselves live in /data.
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
RUN ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx

# All working files are in the volume: config.toml, tg.session, digest.db,
# and agent CLIs under /data/tools and /data/.local.
VOLUME /data
WORKDIR /data

COPY --from=build /src/target/release/tg-digest /usr/local/bin/tg-digest
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY scripts/agents.sh /usr/local/bin/agents
# The entrypoint starts as root only to pick the user. A mounted volume keeps
# the host owner, and the process runs as that owner. When /data belongs to
# root, it is given to user digest (uid 1000).
RUN chmod 755 /usr/local/bin/docker-entrypoint.sh /usr/local/bin/agents

# HOME=/data so CLI login files land on the volume. PATH puts volume bins first.
ENV RUST_LOG=tg_digest=info,warn \
    HOME=/data \
    PATH=/data/tools/bin:/data/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
CMD ["serve"]
