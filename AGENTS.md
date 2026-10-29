# Repository Guidelines

## Project Structure & Module Organization

This Rust 2021 CLI collects Telegram posts through MTProto, summarizes them through Anthropic, and publishes scheduled digests. The service normally runs as a container (`serve`); it is operated through commands sent to Telegram Saved Messages.

- `src/main.rs`: CLI commands, scheduler loop, signal handling.
- `src/config.rs`, `src/db.rs`, `src/schedule.rs`, `src/cancel.rs`: configuration, SQLite state (channels, cursors, run and delivery journals), cron scheduling, cooperative shutdown.
- `src/run.rs`: a full run (collect, summarize, publish), the single lock, and recovery of undelivered runs.
- `src/telegram/`: authentication and post retrieval (`client.rs`), the Telegram folder that defines the channel list (`folder.rs`), delivery with `random_id` (`publish.rs`), and Saved Messages commands (`commands.rs`).
- `src/summarize.rs`, `src/digest.rs`: API requests, response schema, source-URL validation, HTML rendering, and message splitting.
- Unit tests live inside source modules. `config.example.toml` provides configuration; `README.md`, `CLAUDE.md`, and `PLAN.md` describe usage, implementation constraints, and design.
- `Dockerfile` and `compose.yaml` build and run the service; all runtime state lives in the `/data` volume.
- `target/` contains build output; `out/` contains generated previews; `data/` is the local volume and is never committed.

## Build, Test, and Development Commands

- `cargo build --release`: build the optimized executable; Windows requires MSVC build tools.
- `cargo fmt`: format Rust code.
- `cargo clippy --all-targets`: check code for lint issues.
- `cargo test`: run unit tests.
- `Copy-Item config.example.toml config.toml`: initialize local configuration in PowerShell.
- `cargo run -- --config config.toml login`: authenticate the Telegram account.
- `cargo run -- --config config.toml preview`: inspect collected posts without calling Anthropic.
- `cargo run -- --config config.toml run --dry-run`: generate `out/digest.html` using Anthropic without publishing.
- `cargo run -- --config config.toml serve`: run the scheduler and the command listener.
- `docker compose run --rm -it tg-digest login`, then `docker compose up -d --build`: the deployed path.

CLI defaults point at `/data`, the container volume; pass `--config` and adjust `session_file` / `db_file` when running locally. There is no publishing CLI run: only the daemon publishes, through cron or the `/run` command.

## Coding Style & Naming Conventions

Use rustfmt defaults with four-space indentation. Use `snake_case` for modules and functions, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Follow existing `anyhow` error context and `tracing` logging patterns. Update `config.example.toml` when adding settings. Preserve `Cargo.lock`, including the documented `glass_pumpkin` pin.

## Testing Guidelines

Use Rust's built-in `#[test]` framework in `#[cfg(test)] mod tests`. Name tests after behavior, such as `packs_blocks_without_exceeding_limit`. Database tests open an in-memory libsql connection, so they need no fixtures. Add regression tests for changed parsing, rendering, scheduling, collection, or state behavior; no coverage threshold is configured. Before committing, run formatting, Clippy, and tests. Validate live flows with `preview`, then dry-run, then a daemon run with `destination = "me"` before a real channel.

## Commit & Pull Request Guidelines

Git history is unavailable in this checkout. Use concise, imperative commit subjects. PRs should explain the change, relevant issues, configuration impact, and validation performed; include sample output for rendering changes.

## Security & Behavioral Constraints

Never commit credentials, Telegram sessions, or generated runtime state. Secret environment overrides are `TG_API_ID`, `TG_API_HASH`, `TG_PHONE`, and `ANTHROPIC_API_KEY`. Advance channel cursors only in the same transaction that marks a run `ok` or `empty`. Never silently truncate summarization input: a channel that hits `max_posts_per_channel` is excluded from the run with its cursor untouched. Reuse the stored `random_id` for every delivery retry, and never auto-retry a run older than `publish_retry_window_hours`.
