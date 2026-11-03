# Build and runtime are separate stages: the final image has only one binary.
#
# We do not use Alpine/musl on purpose: libsql compiles SQLite from C, and with
# musl that causes extra trouble. rustls is already a dependency, so the image
# does not need OpenSSL — ca-certificates is enough.
FROM rust:1-bookworm AS build
WORKDIR /src

# Copy only the manifests first: the dependency layer survives changes in src/.
# Cargo.lock must be copied — it pins glass_pumpkin 2.0.0-rc0,
# without it the build fails inside grammers-crypto.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# The git commit for `/status` and the start log. CI passes it; a local build
# without it shows only the crate version. It is after the dependency layer, so a
# new commit does not rebuild the dependencies.
ARG GIT_SHA=""
# The file time must change, or cargo reuses the stub.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# All working files are in the volume: config.toml, tg.session, digest.db.
VOLUME /data
WORKDIR /data

COPY --from=build /src/target/release/tg-digest /usr/local/bin/tg-digest

# Docker collects the logs, so we write to stdout without buffering.
ENV RUST_LOG=tg_digest=info,warn
ENTRYPOINT ["tg-digest"]
CMD ["serve"]
