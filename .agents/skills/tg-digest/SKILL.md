---
name: tg-digest
description: >-
  Build, run and debug the tg-digest service — the container with `serve`,
  commands in Saved Messages, the debug CLI commands login/preview/run --dry-run,
  the safe order for checking changes, recovery scenarios and common errors.
  Use it when you need to build the project, run a digest, check which posts are
  collected, debug the schedule or find out why a run failed.
---

# Running and debugging tg-digest

## How it runs

The normal mode is a container with `serve`: the scheduler and the listener for
commands in Saved Messages run in one process. Channels, cursors and the delivery
journal live in the database (`digest.db`), not in the config and not in
`state.json` — that file does not exist any more.

**Only the daemon posts** — on cron or after the `/run` command. The CLI has no
run that posts, on purpose: two processes on one database would post two digests.
The CLI is kept for debugging: `login`, `preview`, `run --dry-run`.

The default paths point to the container volume (`/data`). So locally every
command needs `--config config.toml`, and the config itself needs local
`session_file` and `db_file` paths.

## Before you start

Toolchain: `cargo --version`. If the command is missing, the install steps are in
`README.md` (you need the MSVC linker).

Working config: `config.toml` (not `config.example.toml`). If it is missing, copy
the example and name the fields the owner must fill in: `telegram.api_id`,
`telegram.api_hash`, `telegram.phone`, `anthropic.api_key`. Channels are not in the
config: the owner adds them with `/add` in Saved Messages.

**Never print the content of `config.toml`, `tg.session` or `digest.db`
to a log or a reply** — they hold keys, account access and digest texts.

## Order for checking changes

Go strictly from the cheapest mistake to the most expensive one. Do the next step
only after the previous one is clean.

| Step | Command | What it checks | Cost |
|---|---|---|---|
| 1 | `cargo build` | compilation | — |
| 2 | `cargo test` | answer parsing, rendering, cursors, map-reduce flow | — |
| 3 | `cargo run -- --config config.toml preview` | channels from the database, cursors, album merging, short post filter | free, no LLM call |
| 4 | `cargo run -- --config config.toml run --dry-run` | digest quality: are duplicates merged, are links right | costs tokens, posts nothing |
| 5 | `serve` + `/run` with `destination = "me"` | the full publish: markup, splitting, no link previews | the post goes to Saved Messages |
| 6 | `destination = "@channel"` | — | subscribers see it |

Do not suggest step 6 until the owner clearly says they are happy with the result.

## Login

`login` is an interactive command: it waits for the code from Telegram on stdin,
and with 2FA also for the password. **Do not run it in the background and do not
automate the input.** Ask the owner to run it themselves:

    ! cargo run -- --config config.toml login
    ! docker compose run --rm -it tg-digest login

Success means `tg.session` appears; a second `login` says "Already logged in".

## Container

```bash
docker compose build             # do this first on a clean setup:
                                 # otherwise login and preview run in the old image
docker compose up -d             # start
docker compose logs -f           # watch the runs
docker compose stop              # soft stop (SIGTERM, grace 30s)
docker compose kill              # SIGKILL without grace — only for crash scenarios
```

All state is in `./data`: `config.toml`, `tg.session`, `digest.db`.
Rebuilding while the daemon runs is normal (`up -d --build`): the file is not
locked, as it would be on Windows.

## Commands in Saved Messages

`/help` shows the list. Channel management (`/add`, `/rm`, `/reset`), diagnostics
(`/list`, `/status`), a run (`/run`), a repeat (`/last`) and handling an
undelivered run (`/resume N`, `/abandon N`).

Commands that touch cursors are refused while a run is going — this is not an
error, it is the lock. When there is an undelivered run, only `/run`, `/resume`
and `/abandon` are allowed.

## Debugging the schedule

There is no point in testing with the real cron — you would wait a day. For a
short time use:

```toml
[schedule]
cron = "*/2 * * * *"
```

plus `destination = "me"`. Start `serve` and check that the log has "waiting for
the next run" with the right time, that the run starts, and that Ctrl+C stops the
daemon. After the check **put the real cron back**.

Catch-up (`catch_up`): stop the daemon, wait until a slot is missed, start it
again — the run must happen at once.

More detailed logs: `RUST_LOG=tg_digest=debug`.

## Recovery scenarios (docs/PLAN.md §9.7)

You can check them only live, on real Telegram. The main ones:

- **Delivery after a crash.** A digest of three parts, `docker kill` after the
  first one → after the start the chat has exactly three parts, no duplicates,
  `runs.status = ok`, the cursors moved.
- **The destination is fixed.** The same, but change `destination` between the
  starts → the rest goes to the old chat, the next run goes to the new one.
- **Expiry.** Move `first_send_at` of the chosen undelivered run back by
  `publish_retry_window_hours + 1` hours → there is no auto-retry, one
  notification comes with `/resume` and `/abandon`. Make the change **with the
  container stopped** and only for this run:

  ```bash
  docker compose stop
  sqlite3 data/digest.db "SELECT id, status, first_send_at FROM runs
      WHERE status IN ('summarized','publishing');"       # choose N
  sqlite3 data/digest.db "UPDATE runs
      SET first_send_at = strftime('%Y-%m-%dT%H:%M:%SZ','now','-3 hours')
      WHERE id = N AND status IN ('summarized','publishing');"
  docker compose start
  ```

  The time format must stay RFC3339 (`strftime`, not `datetime`): otherwise the
  run fails when it parses the time from the database. The `status` in `WHERE`
  protects you from changing a finished run.
- **Revoked session.** "Terminate all other sessions" → the container exits with
  a non-zero code, the log asks you to run `login`, `docker ps` shows restarts.

Look at the database only read-only and only with the container stopped:

```bash
sqlite3 data/digest.db "SELECT id, status, error, first_send_at FROM runs ORDER BY id DESC LIMIT 5;"
sqlite3 data/digest.db "SELECT id, title, enabled, last_msg_id, last_error FROM channels;"
```

## Common errors

**`link.exe not found`** — VS Build Tools are not installed, see `README.md`.

**Compile errors in `src/telegram/`** — almost always grammers API drift. The
crate is pre-1.0, and 0.10 rewrote the connection scheme (`SenderPool` +
`Client::new(handle)`, `SqliteSession::open`, updates from `pool.updates`). Check
signatures at <https://docs.rs/grammers-client/0.10.0/>, not in search examples —
they target the old API.

**`not logged in to Telegram`** — run `login`.

**`channel excluded from the run`** — it hit `max_posts_per_channel` or the
channel cannot be read. The cursor did not move, the next run reads the rest;
raise the limit or reset the channel with `/reset <id>`.

**FLOOD_WAIT** — too many requests. Raise `pause_between_channels_ms` and do not
set cron more often than once every few hours.

**HTTP 400 from the Anthropic API** — most often the request has a parameter that
`claude-opus-5` does not accept (`thinking`, `budget_tokens`, `temperature`), or
an object in the JSON schema has no `additionalProperties: false`.

**`the answer was cut at max_tokens`** — raise `anthropic.max_tokens`.

**`summaries of … parts do not fit in one request`** — map-reduce could not
condense the input even with intermediate rounds. The cursors did not move; lower
`lookback_hours` or the number of channels.

**Compile error inside `grammers-crypto`** (`safe_prime::check`, `BigUint`) —
cargo pulled `glass_pumpkin 2.0.0-rc1` instead of `rc0`. The fix is
`cargo update -p glass_pumpkin --precise 2.0.0-rc0`; this is why `Cargo.lock` is committed.

**`no method named html`** — the `html` feature of `grammers-client` is off
(it is not in `default`), check `Cargo.toml`.

## What not to do

- Do not silently cut the input when there are many posts: map-reduce exists for
  that, and if it also fails, fail with an error. Silently losing half of the
  channels looks like a working digest.
- Do not edit `digest.db` by hand to "restart a run": for a repeat run there is
  `--dry-run`, to reset a channel there is `/reset`, to cancel an undelivered run
  there is `/abandon`.
- Do not post from the CLI and do not add such a command: only the daemon posts.
- Do not add a date suffix to the model id.
