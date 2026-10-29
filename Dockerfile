# Сборка и рантайм разнесены: в финальный образ едет один бинарник.
#
# Alpine/musl не берём осознанно: libsql компилирует C-шный SQLite, и с musl
# это лишние приключения. rustls уже в зависимостях, поэтому OpenSSL в образе
# не нужен — хватает ca-certificates.
FROM rust:1-bookworm AS build
WORKDIR /src

# Сначала только манифесты: слой с зависимостями переживает правки в src/.
# Cargo.lock копируется обязательно — в нём закреплён glass_pumpkin 2.0.0-rc0,
# без него сборка падает внутри grammers-crypto.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# Дата файла должна поменяться, иначе cargo переиспользует заглушку.
RUN touch src/main.rs && cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Все рабочие файлы — в volume: config.toml, tg.session, digest.db.
VOLUME /data
WORKDIR /data

COPY --from=build /src/target/release/tg-digest /usr/local/bin/tg-digest

# Логи собирает Docker, поэтому пишем в stdout без буферизации.
ENV RUST_LOG=tg_digest=info,warn
ENTRYPOINT ["tg-digest"]
CMD ["serve"]
