# tg-digest

A Rust 2021 daemon. It reads a list of Telegram channels through MTProto, makes a
summary of the posts with the Claude Messages API, and posts a digest back to
Telegram on a cron schedule. Normally it runs in a container with `serve`. You
control it with commands to a Telegram bot, and the bot sends you the digests.

The full design and the order of work are in [docs/PLAN.md](docs/PLAN.md). How to
use it is in [README.md](README.md).

## Project structure

| File | What it does |
|---|---|
| `src/main.rs` | CLI (`login` / `preview` / `run --dry-run` / `serve`), scheduler, signals |
| `src/config.rs` | `config.toml` (all settings and secrets), validation |
| `src/db.rs` | SQLite: channels with cursors, journal of runs and delivered parts |
| `src/run.rs` | one full run (collect, summarize, publish), the lock, recovery of undelivered runs |
| `src/cancel.rs` | safe shutdown on SIGTERM / Ctrl+C |
| `src/schedule.rs` | croner: next trigger, catch-up of missed slots |
| `src/telegram/client.rs` | connection, login, reading posts by cursor |
| `src/telegram/channels.rs` | finding a channel for `/add`, reading titles and usernames before each run |
| `src/telegram/publish.rs` | sending parts by `random_id`, delivery journal |
| `src/telegram/photos.rs` | photos for topics: download by the personal account, files in `/data/photos` until the run is done |
| `src/telegram/bot.rs` | the bot: login with the token, the chat with the owner, notifications |
| `src/telegram/commands.rs` | commands to the bot (`/add`, `/rm`, `/run`, `/status`, …), only from the owner |
| `src/models.rs` | the model catalog: each provider's model list and limits from its API, kept in `settings` |
| `src/summarize.rs` | HTTP calls to the model API (Anthropic, Gemini or OpenRouter, `[providers]`), map-reduce |
| `src/digest.rs` | JSON schema of the answer, link checks, Telegram-HTML rendering, splitting |
| `Dockerfile`, `compose.yaml` | image and run; all state is in the `/data` volume |

- `config.example.toml` is the config template.
- `.agents/skills/tg-digest/` is the guide for running and debugging the service.
- `target/` is build output, `out/` has generated previews, `data/` is the local
  volume. None of them are committed.

## Commands

- `cargo build --release` — build the release binary. On Windows you need the MSVC build tools.
- `cargo fmt`, `cargo clippy --all-targets` — format, lint.
- `Copy-Item config.example.toml config.toml` — create a local config in PowerShell.
- `cargo run -- --config config.toml login` — log in to the Telegram account.
- `cargo run -- --config config.toml preview` — see the collected posts, no Anthropic call.
- `cargo run -- --config config.toml run --dry-run` — make `out/digest.html` with Anthropic, post nothing.
- `cargo run -- --config config.toml serve` — run the scheduler and the command listener.
- `docker compose run --rm -it tg-digest login`, then `docker compose up -d --build` — the normal deploy.
- `make update` — pull the CI image, start it, follow the logs; `make restart` —
  restart the container, follow the logs.

The CLI default paths point to `/data`, the container volume. When you run
locally, pass `--config` and change `session_file` / `db_file`.

**Check before every commit:** `cargo fmt && cargo clippy --all-targets`. The project has no tests.

## Language

All text in the repository is in simple English (level B1): docs, comments, log
messages, bot replies, and the default prompts. Use short sentences and common
words.

## Rules

**Secrets.** `config.toml` and `tg.session` are in `.gitignore` and must stay
there. `tg.session` gives full access to the Telegram account — it is not "just a
cache". Only `config.example.toml` goes into the repository. All settings, the
secrets too, come only from `config.toml` (in a container: `data/config.toml`,
mounted as `/data`); there are no environment overrides. When you add a setting,
add it to the example too. When you add a module, add it to the table above.
Never commit runtime state.

**The channel list lives only in the database.** It is managed with `/add` and
`/rm` sent to the bot. There is no Telegram folder, no `[[channels]]` in the
config, and no sync step. Titles and usernames are read again before every run
(`refresh_channels`, one `channels.getChannels` call); this never changes the
list. Otherwise a renamed channel would stay in the digest forever with an old
link.

**Two Telegram clients, two jobs.** The personal account (`[telegram.app]`,
`tg.session`) only reads: it collects posts, resolves `/add`, refreshes names. A
bot cannot do that — it reads only channels where it is an admin. The bot
(`[telegram.bot]`, `bot.session`, logs in with the token on start) only talks: it takes
commands, replies, and sends digests and notifications. The bot accepts commands
only from the owner (the personal account); everyone else is ignored (logged with
the user id). There is one user: digests and notifications go only to the owner
(or digests to the channel in `destination`). A
bot can write to a user only after the user wrote to it, so the owner sends
`/start` once; the chat is then kept in `bot.session`.

**grammers 0.10.** The crate is pre-1.0, and its API changed a lot after 0.7–0.9.
You connect through `SenderPool`: `runner` runs as a separate task, `Client::new`
takes `handle`, updates come from `pool.updates`. The session is
`SqliteSession::open`, not a file session. Check signatures at
<https://docs.rs/grammers-client/0.10.0/>. Do not trust memory or examples from
search — almost all of them use the old API.

**Model providers.** `[providers.<provider>]` has the key and shared settings. No model id is written in the code, and models do not have to be in the config: the limits (input, output, strict JSON schema) come from the provider's model list (`models.rs`, read once a day, kept in `settings`). `[providers.<provider>.models."<model id>"]` only changes a setting by hand; the order is model table, model list, provider table, defaults. A model the provider does not have is refused at `/model`, not found as a 404 in a run. A provider is `anthropic`, `gemini`, `openrouter` or `openai`. OpenAI uses the chat format of OpenRouter, but with `max_completion_tokens`, a strict schema, and `reasoning_effort` only when `effort` is set. `model` and `fallback` name models as `provider/model id` (only the first `/` splits, OpenRouter ids have their own). When the model is overloaded (5xx after the retries) or its limit is used up, the bot tells the owner in plain words and offers `/fallback` (the next model in `fallback`). A `/run` gets this text as the reply, not as a second message. Nothing switches by itself. The choice is kept in the `settings` table (`/model`), so the config value is only the default.

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

**No story twice in a row.** Each run saves its topics (`runs.digest_topics`). The
next run adds the topics of the last delivered digest to the system prompt and asks
the model not to repeat them, only to write what is really new.

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
(nothing is posted), then the daemon with `destination = "bot"` and `/run` to
the bot. Posting to a real channel is the last step. The CLI has no run
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

## Commits and pull requests

`main` is protected: no direct pushes, no force pushes. Every change goes through a
branch and a pull request, and the `check` (cargo) job must
pass before the merge.

Write short commit subjects in the imperative mood. A PR should explain the change,
related issues, the effect on the config, and how you checked it. For rendering
changes, add sample output.

CI has two workflows:

- `.github/workflows/cargo.yml` — on every push to a branch except `main`:
  `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo build --release`, all with `--locked`.
- `.github/workflows/docker.yml` — a push to `main` builds and
  publishes one image to `ghcr.io/vlaship/tg-digest`, tagged `latest` and with the
  UTC build time (`20261103-041200`). The same time is built into the binary as the
  version (`/status`). There are no `v*` tags. On branches and PRs it runs
  only by hand (Actions → docker → Run workflow); a manual run only builds, unless
  you tick "publish".

CI costs money: push only when the owner asks, and test locally first
(`docker compose up -d --build`).
