# tg-digest

A Rust 2021 daemon. It reads a list of Telegram channels through MTProto, makes a
summary of the posts with the Claude Messages API, and posts a digest back to
Telegram on a cron schedule. Normally it runs in a container with `serve`. You
control it with commands in Telegram Saved Messages.

The full design and the order of work are in [docs/PLAN.md](docs/PLAN.md). How to
use it is in [README.md](README.md).

## Project structure

| File | What it does |
|---|---|
| `src/main.rs` | CLI (`login` / `preview` / `run --dry-run` / `serve`), scheduler, signals |
| `src/config.rs` | `config.toml` + env overrides for secrets, validation |
| `src/db.rs` | SQLite: channels with cursors, journal of runs and delivered parts |
| `src/run.rs` | one full run (collect, summarize, publish), the lock, recovery of undelivered runs |
| `src/cancel.rs` | safe shutdown on SIGTERM / Ctrl+C |
| `src/schedule.rs` | croner: next trigger, catch-up of missed slots |
| `src/telegram/client.rs` | connection, login, reading posts by cursor |
| `src/telegram/folder.rs` | the Telegram folder as the channel list: read, check, sync |
| `src/telegram/publish.rs` | sending parts by `random_id`, delivery journal |
| `src/telegram/commands.rs` | Saved Messages commands (`/add`, `/run`, `/status`, …) |
| `src/summarize.rs` | HTTP calls to the Anthropic Messages API, map-reduce |
| `src/digest.rs` | JSON schema of the answer, link checks, Telegram-HTML rendering, splitting |
| `Dockerfile`, `compose.yaml` | image and run; all state is in the `/data` volume |

- Unit tests are inside the source modules.
- `config.example.toml` is the config template.
- `.agents/skills/tg-digest/` is the guide for running and debugging the service.
- `target/` is build output, `out/` has generated previews, `data/` is the local
  volume. None of them are committed.

## Commands

- `cargo build --release` — build the release binary. On Windows you need the MSVC build tools.
- `cargo fmt`, `cargo clippy --all-targets`, `cargo test` — format, lint, test.
- `Copy-Item config.example.toml config.toml` — create a local config in PowerShell.
- `cargo run -- --config config.toml login` — log in to the Telegram account.
- `cargo run -- --config config.toml preview` — see the collected posts, no Anthropic call.
- `cargo run -- --config config.toml run --dry-run` — make `out/digest.html` with Anthropic, post nothing.
- `cargo run -- --config config.toml serve` — run the scheduler and the command listener.
- `docker compose run --rm -it tg-digest login`, then `docker compose up -d --build` — the normal deploy.

The CLI default paths point to `/data`, the container volume. When you run
locally, pass `--config` and change `session_file` / `db_file`.

**Check before every commit:** `cargo fmt && cargo clippy --all-targets && cargo test`.

## Language

All text in the repository is in simple English (level B1): docs, comments, log
messages, bot replies, and the default prompts. Use short sentences and common
words.

## Rules

**Secrets.** `config.toml` and `tg.session` are in `.gitignore` and must stay
there. `tg.session` gives full access to the Telegram account — it is not "just a
cache". Only `config.example.toml` goes into the repository. The secret
environment overrides are `TG_API_ID`, `TG_API_HASH`, `TG_PHONE` and
`ANTHROPIC_API_KEY`. When you add a setting, add it to the example too. When you
add a module, add it to the table above. Never commit runtime state.

**The truth about channels is in the database and in the Telegram folder,** not in
the config. `[[channels]]` is only a seed for an empty database. After the first
start, changes there do nothing. Titles and usernames are read again before every
run (`refresh_channels`) with one `channels.getChannels` call — with or without a
folder. Otherwise a renamed channel would stay in the digest forever with an old
link.

**grammers 0.10.** The crate is pre-1.0, and its API changed a lot after 0.7–0.9.
You connect through `SenderPool`: `runner` runs as a separate task, `Client::new`
takes `handle`, updates come from `pool.updates`. The session is
`SqliteSession::open`, not a file session. Check signatures at
<https://docs.rs/grammers-client/0.10.0/>. Do not trust memory or examples from
search — almost all of them use the old API.

**Anthropic API.** The model id comes from the config as it is. Do not add a date
suffix. On `claude-opus-5` thinking is on by default — do not send `thinking`,
`budget_tokens` or `temperature` (you get 400). For structured output use
`output_config.format` with `type: "json_schema"`. Every object in the schema must
have `additionalProperties: false`.

**Never cut the input silently.** Input bigger than `LARGE_INPUT_CHARS` goes to
map-reduce (`summarize.rs`):

- The input is split into parts. Each part is condensed by its own request. Then
  the parts are merged with the shared system prompt.
- Parts are split by channel. A channel is split only if it alone does not fit
  the budget.
- If the summaries together still do not fit in one request, they are merged in
  extra rounds (up to `MAX_REDUCE_STAGES`).
- Before the final request, the whole prompt is checked (`ensure_fits`). If it
  does not fit, the run fails with a clear error. It does not send a bad request
  and does not drop topics. The cursors do not move, so the posts go into the next
  run.
- Post links must survive the map step. Otherwise the final step has nothing to
  put in `sources`, and everything fails the check.
- `max_posts_per_channel` is a safety limit. A channel that hits it is removed
  from the run completely, and its cursor does not move.

**A repost is marked with its source.** Without the "repost from: …" label the
model sees a repost and its original as two news items and does not merge them.

**Cursors move in one transaction with the `ok` or `empty` status.** Until then
they are stored in `runs.cursors` and do not change what is read. So a failed
publish does not "eat" posts that nobody saw. An empty run is also a success: the
posts were read.

**Resend only with the saved `random_id`.** It is created once, when a part is
written to the journal. `RANDOM_ID_DUPLICATE` means "delivered", not an error.
Telegram does not document how long it remembers `random_id`, so after
`publish_retry_window_hours` the daemon does not retry by itself. The owner
decides (`/resume`).

**Order for checking changes.** First `preview` (no tokens), then `run --dry-run`
(nothing is posted), then the daemon with `destination = "me"` and `/run` from
Saved Messages. Posting to a real channel is the last step. The CLI has no run
that posts: only the daemon posts (cron or `/run`). Otherwise two processes on one
database would post two digests.

## Build problems we already hit

**`Cargo.lock` is committed.** `grammers-crypto` needs `glass_pumpkin = "2.0.0-rc0"`.
But semver rules for pre-releases let cargo take `2.0.0-rc1`. That version changed
the API (`safe_prime::check` now returns `Result`) and moved `num-bigint` to 0.5, so
the build fails inside the dependency. The version is pinned in the lock file:

```
cargo update -p glass_pumpkin --precise 2.0.0-rc0
```

If the build suddenly breaks in `grammers-crypto` after `cargo update`, this is
the reason.

**The `html` and `parse_invite_link` features of grammers-client are turned on
explicitly.** `InputMessage::html()` is behind `#[cfg(feature = "html")]`, and
`Client::accept_invite_link` is behind `parse_invite_link`. Neither is in
`default`. Without them you get the error "no method named `html`".

**Use the same `libsql` that `grammers-session` uses** (0.9,
`default-features = false`, feature `core`). Otherwise a second SQLite gets into
the binary.

## Code style

Use rustfmt defaults with four-space indentation. Use `snake_case` for modules and
functions, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Follow
the existing `anyhow` error context and `tracing` logging patterns.

## Tests

Use Rust's built-in `#[test]` in `#[cfg(test)] mod tests`. Name a test after the
behavior it checks, for example `packs_blocks_without_exceeding_limit`. Database
tests open an in-memory libsql connection, so they need no fixtures. Add
regression tests when you change parsing, rendering, scheduling, collection or
state. There is no coverage target.

## Commits and pull requests

Write short commit subjects in the imperative mood. A PR should explain the change,
related issues, the effect on the config, and how you checked it. For rendering
changes, add sample output. CI (`.github/workflows/docker.yml`) builds the Docker
image on every push and PR. Pushes to `main` and `v*` tags also publish it to
`ghcr.io/vlaship/tg-digest`; PRs only build it.
