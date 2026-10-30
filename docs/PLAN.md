# tg-digest — a Rust service that summarizes Telegram channels

## Context

We need a daemon service that **on a schedule (cron)** reads a list of Telegram
channels, makes a clear digest from them with Claude, and posts it back to
Telegram. Everything is controlled from one file, `config.toml`: channels,
prompts, API keys, schedule.

The key limit that shaped the architecture: **a bot cannot read other people's
channels** — it must be added as an admin to each one. So reading goes through
**MTProto with a personal account** (`grammers-client`), and the same account
posts the digest — one session for everything, no BotFather.

Format requirement: the digest is grouped **by topic**, and if several channels
write about the same event, the items are **merged into one** with a list of all
sources.

The project is new: `C:\vs\prj` is a sandbox with the subprojects `go/ java/ py/ ts/`,
and we add `rust/`.

### Decisions agreed with the user
| Question | Decision |
|---|---|
| Reading | MTProto, personal account (`grammers-client` 0.10) |
| Stack | Rust |
| Toolchain | winget: rustup + MSVC Build Tools |
| Publishing | the same account → Saved Messages (`me`) or your own channel |
| LLM | Anthropic Messages API, `claude-opus-5` |
| Running | a daemon with a cron schedule in the config |
| Configuration | one `config.toml`: channels, prompts, keys, schedule |
| Format | grouping by topic + merging duplicates across channels |

### What to know about the dependencies
- `grammers-client` / `grammers-session` **0.10.0** (2 July 2026, by Lonami, the author
  of Telethon). Pure Rust, no TDLib and no C/C++ dependencies.
- The crate is **pre-1.0, and the API was rewritten in 0.10** — checked on docs.rs:
  you connect through `SenderPool::new(session, api_id)` (fields `runner` / `handle` /
  `updates`), `runner.run()` runs as a separate task, and `Client::new` takes
  `handle`. The session is `SqliteSession::open`; the file-based
  `Session::load_file_or_create` is gone. `Chat` was renamed to `Peer`.
  The only reliable source of signatures is
  <https://docs.rs/grammers-client/0.10.0/>; check there, not from memory and not
  in old examples from Google.
- `croner` **4.0.0** — only parsing and calculating the next trigger
  (`Cron::from_str("0 9 * * *")` → `find_next_occurrence(&now, false)`); we write the
  wait loop ourselves. Chosen instead of `tokio-cron-scheduler`: fewer dependencies
  and full control over logging, job errors and catching up missed runs.
- There is no official Anthropic SDK for Rust → we call the Messages API directly
  with `reqwest` (this is the normal way for languages without an SDK).

---

## Step 0. Toolchain (once, outside the repository)

```powershell
winget install --id Rustlang.Rustup --exact
winget install --id Microsoft.VisualStudio.2022.BuildTools --exact `
  --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```
Both IDs were checked in the local winget (rustup 1.29.1, BuildTools 17.14.39). After
the install, **restart PowerShell** and check: `cargo --version`, `rustc --version`.

---

## Step 1. Project skeleton

`C:\vs\prj\rust\tg-digest\`

```
Cargo.toml
.gitignore            # config.toml, tg.session, state.json, out/, target/
config.example.toml   # template without secrets — this one is committed
config.toml           # working config with keys — NOT committed
src/
  main.rs             # clap: login | preview | run | serve
  config.rs           # serde deserialization of config.toml + env overrides
  schedule.rs         # croner: wait loop, catch-up of missed runs
  state.rs            # last_msg_id per channel + last_run, atomic write of state.json
  telegram/
    mod.rs
    client.rs         # connection, login, resolve, post selection
    publish.rs        # sending the digest, splitting by the 4096 limit
  summarize.rs        # HTTP client for the Messages API
  digest.rs           # prompt filling, answer model, rendering to Telegram HTML
```

### Dependencies (Cargo.toml)
```toml
grammers-client  = "0.10"
grammers-session = "0.10"
tokio     = { version = "1", features = ["rt-multi-thread", "macros", "time", "fs", "signal"] }
reqwest   = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde     = { version = "1", features = ["derive"] }
serde_json = "1"
toml      = "0.8"
clap      = { version = "4", features = ["derive"] }
croner    = "4"
chrono    = { version = "0.4", features = ["serde"] }
chrono-tz = "0.10"   # schedule time zone from the config
anyhow    = "1"
tracing   = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
rpassword = "7"      # 2FA password input without echo
```

---

## Step 2. `config.toml` — all settings in one place

```toml
[schedule]
cron        = "0 9 * * *"        # 5 fields, standard cron; every day at 09:00
timezone    = "America/New_York"    # IANA name, parsed by chrono-tz
catch_up    = true               # if a run was missed (the PC was asleep), catch up on start
run_on_start = false             # run right after the daemon starts, for debugging

[telegram]
api_id   = 123456                # my.telegram.org
api_hash = "..."
phone    = "+1..."
session_file = "tg.session"

destination           = "me"     # "me" = Saved Messages, or "@my_channel"
lookback_hours        = 24
max_posts_per_channel = 100
min_post_chars        = 30       # drop "media only" and one-word posts
pause_between_channels_ms = 1500

[anthropic]
api_key    = "sk-ant-..."
model      = "claude-opus-5"
effort     = "high"
max_tokens = 16000

[prompts]
system = """
You are the editor of a daily news digest made from Telegram channels.
Group the material by topic. If several channels write about THE SAME event,
merge them into ONE item and list all sources. Do not repeat one news item in different topics.
Write in English, stick to the facts, no filler, no opinions, no advertising language.
"""

user_template = """
Digest for the period: the last {{lookback_hours}} h (as of {{now}}).
Posts: {{post_count}} from {{channel_count}} channels.

{{posts}}
"""

[[channels]]
username = "durov"
title    = "Durov"               # optional, the label in the digest

[[channels]]
username = "example_channel"
enabled  = false                 # turn a channel off for a while without removing it from the config
```

**Filling the template** is a simple `str::replace` of the placeholders
`{{lookback_hours}} {{now}} {{post_count}} {{channel_count}} {{posts}}`. There is no
reason to pull in a template engine (tera/handlebars).

**Secrets.** The keys are in `config.toml`, as requested. Two things must follow:
`config.toml` goes into `.gitignore` **before the first commit**, and
`config.example.toml` with empty values is what gets committed. Also, `config.rs`
gives an env override: if `ANTHROPIC_API_KEY` / `TG_API_ID` / `TG_API_HASH` /
`TG_PHONE` is set, it overrides the value from the TOML. So the same config can be
used on another machine or in CI without any change.

---

## Step 3. `login` — one-time login (`telegram/client.rs`)

1. `SqliteSession::open(&cfg.telegram.session_file).await?` → `SenderPool::new(session, api_id)`
   → `tokio::spawn(runner.run())` → `Client::new(handle)`.
2. If `!client.is_authorized().await?`:
   `request_login_code(phone).await?` → code from stdin → `sign_in(&token, &code).await?`.
3. Handle `SignInError::PasswordRequired(token)` → `rpassword::prompt_password` →
   `check_password(token, pwd).await?`. Without this branch accounts with 2FA cannot log in.
4. There is no need to save the session separately: `SqliteSession` writes to disk by itself.

> ⚠️ `tg.session` gives **full access to the account**, the same as a logged-in client.
> It is in `.gitignore` from the first commit; never commit it or copy it anywhere.

---

## Step 4. Collecting posts (`telegram/client.rs`)

For each channel with `enabled != false`:
- `client.resolve_username(&username).await?` → `Peer` (it can return `None` — the
  channel was renamed or deleted: log it and go on, do not fail the whole run).
- `client.iter_messages(peer).limit(max_posts_per_channel)` — goes from new to old;
  we stop when `msg.date() < since` **or** `msg.id() <= last_seen[channel]`.
- A pause of `pause_between_channels_ms` between channels + backoff on FLOOD_WAIT.

Details to handle from the start (or the digest will be wrong):
- **Albums**: several messages with one `grouped_id` are one publication. Merge them
  into one `Post`, take the non-empty caption, link to the first message of the group.
- **Reposts**: mark the source channel, so the duplicate merging below does not see
  them as separate news.
- **Short/media posts**: drop them if the text is shorter than `min_post_chars`.

Model:
```rust
struct Post { channel: String, title: String, msg_id: i32,
              date: DateTime<Utc>, text: String, url: String }
```
`url` = `https://t.me/{username}/{msg_id}` — this is what goes into the digest sources.

### state.json (`state.rs`)
```json
{ "last_run": "2026-09-08T06:00:00Z",
  "channels": { "durov": { "last_msg_id": 1234 } } }
```
The write is atomic (to a temp file + `rename`), otherwise a stopped run leaves broken
JSON and the next start reads the whole history again. The scheduler also uses
`last_run` — see catch-up.

---

## Step 5. Summarizing (`summarize.rs` + `digest.rs`)

One request `POST https://api.anthropic.com/v1/messages` for the whole batch of posts.

Headers: `x-api-key`, `anthropic-version: 2023-06-01`, `content-type: application/json`.

Body:
```jsonc
{
  "model": "claude-opus-5",
  "max_tokens": 16000,
  "output_config": { "effort": "high", "format": { /* json_schema, see below */ } },
  "system": "<cfg.prompts.system>",
  "messages": [{ "role": "user", "content": "<cfg.prompts.user_template with placeholders filled>" }]
}
```

- **Thinking is on by default** on `claude-opus-5` — we do not send the `thinking` parameter.
  Do not send `budget_tokens` (400) and do not set `temperature` (400 on this model).
- **Structured outputs** through `output_config.format` — we ask for JSON, not ready
  markdown: we render Telegram-HTML ourselves, this is more reliable than fixing the
  model's markup.
  ```jsonc
  { "topics": [ { "title": "...", "summary": "...",
                  "sources": [ { "channel": "...", "url": "..." } ] } ],
    "tldr": ["...", "..."] }
  ```
  The schema lives in the code (it is the parsing contract), the text instructions are
  in `[prompts]` of the config.
- **Size**: if the total post text is > ~400,000 characters, switch to map-reduce
  (first condense by channel, then merge) — otherwise one request is more than enough,
  the model context is 1M.
- One run a day, so **prompt caching is useless** (TTL 5 minutes) — do not plan for it.
- Answer handling: check `stop_reason` **before** reading `content` (`"refusal"` is
  possible), parse `content[] | type=="text"` with `serde_json`, not with regular
  expressions.
- Errors: separate branches for 429 (retry by `retry-after`), 5xx (backoff), 400/401
  (fail with a clear message) — one shared `is_err()` branch loses the difference
  between "retry" and "wrong key".

**Cost estimate**: 20 channels × ~30 posts × ~500 chars ≈ 300k characters ≈
120k input tokens. On `claude-opus-5` ($5/$25 per 1M) this is ≈ **$0.6–0.8 per run**,
~$20/month with a daily run. The model and `effort` are in the config — you can lower
them there if you want, no code change is needed.

---

## Step 6. Publishing (`telegram/publish.rs`)

- Render `Digest` → Telegram HTML: `<b>topic</b>`, text, a line of sources
  `<a href="url">channel</a> · <a href="url">channel</a>`.
- **Limit of 4096 characters per message** — cut at topic borders, not blindly by
  characters; a long digest goes out as a series of messages.
- Sending: `client.send_message(peer, InputMessage::html(chunk).link_preview(false))`.
  Previews are off — otherwise Telegram shows the picture of the first link over the
  whole post.
- Target: `destination = "me"` → the peer of the current account (Saved Messages);
  otherwise `resolve_username(&dest)`.

---

## Step 7. Scheduler (`schedule.rs`) and run modes

CLI:
| Command | What it does |
|---|---|
| `tg-digest login` | one-time login to Telegram |
| `tg-digest preview` | collect posts and show statistics, **no LLM call** |
| `tg-digest run [--dry-run]` | one run here and now |
| `tg-digest serve` | daemon: runs on `[schedule].cron` |

The `serve` loop:
1. `Cron::from_str(&cfg.schedule.cron)?` — 5-field format; the time zone from the config
   through `chrono_tz::Tz::from_str`, we calculate time in it (otherwise "09:00" moves
   to UTC).
2. **Catch-up**: on start, if `catch_up = true` and the next trigger after
   `state.last_run` is already in the past, run at once (the PC was asleep or the
   daemon was off). Plus `run_on_start` for debugging.
3. `let next = cron.find_next_occurrence(&now_tz, false)?` → `tokio::time::sleep_until(...)`.
   Do **not** sleep with one long sleep until next, but in ticks of ~60 s with a
   recheck: on a laptop a long sleep survives the machine sleeping in unpredictable
   ways and causes a missed run.
4. The run is in `tokio::select!` with `tokio::signal::ctrl_c()` — Ctrl+C stops the
   daemon without killing a send in the middle.
5. **A run error does not break the daemon**: we log it, do not move `last_run`, and
   wait for the next trigger. Fail completely only on fatal errors (not logged in,
   broken config).

Auto-start of the daemon on Windows is a Task Scheduler task "at log on":
```powershell
schtasks /Create /TN "tg-digest" /SC ONLOGON `
  /TR "C:\vs\prj\rust\tg-digest\target\release\tg-digest.exe serve"
```
The digest schedule itself lives in `config.toml`, not in Task Scheduler — changing
the cron does not need a new task. Logs go to a file through `tracing-subscriber`.

---

## Verification

The order is exactly this — each step is checked without a risk of spamming the channel.
Locally every command needs `--config config.toml`: the default paths point to the
container volume (`/data`). The CLI has no real run that posts — only the daemon
posts, so items 6–9 already go through `serve` and `/run`.

1. `cargo --version` — the toolchain is installed, the linker is found.
2. `cargo build` — compilation; grammers 0.10 API drift also shows up here, check it
   against docs.rs.
3. `cargo run -- login` — enter the code from Telegram (and the 2FA password if it is on).
   Success: `tg.session` is created, a second start does not ask for the code.
4. `cargo run -- preview` — **no LLM call**: prints how many posts were collected for
   each channel and the first lines. We check the folder content, the cursors, album
   merging, the short post filter.
5. `cargo run -- run --dry-run` — a full run with the LLM, the result goes to
   stdout/`out/digest.html`, **nothing is sent and nothing is written to the
   database**. We check the main things: are duplicates across channels merged, are
   the links to the original posts right.
6. `serve` with `destination = "me"`, then `/run` in Saved Messages — the first real
   post goes there too. We check the HTML markup, the splitting of a long digest, no
   previews, the `/status` reply with tokens.
7. `/run` again right away — it must say "posts 0" and leave the status `empty`
   (a check of the cursors in the database).
8. Scheduler: for a short time `cron = "*/2 * * * *"` + `destination = "me"` — make
   sure the run starts on schedule, is logged, and Ctrl+C stops the daemon correctly.
   Then put the real cron back.
9. Catch-up: stop the daemon, wait until a slot is missed, start it again with
   `catch_up = true` — the run must happen at once.
10. Only after that — switch `destination` to the real channel and start the
    container (`docker compose up -d --build`), see items 11–26 in 9.7.

## Risks

- **grammers 0.x**: signatures may not match what we expect; the fix is to check
  against docs.rs in step 2, but plan for a couple of compile rounds.
- **Secrets in `config.toml`**: `.gitignore` before the first commit, only
  `config.example.toml` goes into the repository.
- **`tg.session` = access to the account**: also into `.gitignore`.
- **Telegram limits**: once a day + pauses between channels are safe; if you set cron
  to run every few minutes, there is a risk of limits on the account.

---

## Step 8. What the plan missed (review 2026-09-15)

Found while comparing the plan with the code. This is not "not finished", but "not
thought about": items that are not in steps 1–7 at all. Priority: **[A]** breaks
production or spoils the digest, **[B]** inconvenient/expensive, **[C]** some day.

### Post selection

- **[A] Private channels.** The config accepts only `username`, and `resolve_username`
  works only for public channels. A channel from an invite link (`t.me/+…`) that the
  account follows cannot be added. Needed: `id = -100…` as an alternative to `username`
  (take the peer from the dialogs / `PeerRef` by id), link `https://t.me/c/{id}/{msg_id}`.
- **[A] `max_posts_per_channel` cuts the input silently** — a direct break of the rule
  from AGENTS.md. If a channel gave 150 posts in the window, the 50 oldest are lost, and
  `last_msg_id` jumps over them forever. At least a `warn!` when the limit is hit; the
  right way — the limit only as a safety net against reading the whole history, not as a
  digest cut.
- **[A] Catch-up loses posts.** Reading stops on `date < since` **or** `id <= last_seen`,
  and `since = now − lookback_hours`. After the PC sleeps for three days, `catch_up` runs
  the digest, but only for the last 24 h — two days of posts are lost, even though
  `last_msg_id` sees them. Solution: if `last_run` exists, window = `max(lookback_hours, now − last_run)`.
- **[B] `last_msg_id` moves by the filtered posts, not by the highest id read.** For an
  album this is the `msg_id` of the first part; the other parts are above the pointer and
  are read again on the next run, but without the first one (there is a `break`) — if the
  caption was not in the first part, the album goes into the digest a second time. Move
  the pointer to the highest id seen, regardless of the filter.
- **[B] Links inside the text are lost.** `message.text()` gives plain text, URLs from a
  text entity ("more — here") are lost. `message.html()` would help the model (the `html`
  feature is already on), or a list of URLs from entities as a separate line.
- **[C] Edited posts.** A pointer by id does not read edits again. We do not support this
  on purpose; record it as a limitation.

### Model answer

- **[A] Validation of `sources[].url`.** The prompt says "do not make up URLs", the code
  does not check. Compare every `url` with the set of URLs of collected posts; drop
  unknown ones with a `warn!`. This also protects from prompt injection in post text:
  structured output + checked links = the model cannot send the reader to any address.
- **[A] Empty digest.** `topics = []` now goes to Telegram as a message with only a
  title. Do not post it; move the state (the posts were read, there was just nothing
  important).
- **[B] Log `usage`** (`input_tokens`, `output_tokens`) from the answer — otherwise
  there is nothing to check the "$0.6–0.8 per run" estimate against. Also: watch whether
  we hit `max_tokens` because of thinking with `effort = "high"` — by `stop_reason` in
  the logs.
- **[B] `escape` does not escape `"`** — a URL with a quote breaks `href`. Solved by the
  validation above (we let through only URLs from our own set).

### Daemon and reliability

- **[A] The working folder of Task Scheduler is `System32`.** `config.toml`, `state.json`,
  `tg.session` are relative paths → an ONLOGON task will not find the config. Either
  absolute `--config` / `--state` and an absolute `session_file`, or start it through
  `cmd /c "cd /d C:\vs\prj\rust\tg-digest && target\release\tg-digest.exe serve"`.
  Must be checked in step 10 of the verification.
- **[A] The session is revoked (`AUTH_KEY_UNREGISTERED`, "terminate all sessions" in
  Telegram).** Now the daemon would silently fail on every run forever. Detect the auth
  error and **exit** with a clear message — this is fatal, not "an error of one run".
- **[A] Error notification.** The night run failed — we learn about it from the logs a
  week later. We already have a Telegram client: on a run error send a short message to
  `me` ("digest not built: <error>"). Cheaper than any monitoring.
- **[B] Two instances at the same time** (`serve` from Task Scheduler + a manual `run`):
  both read `state.json`, both post, one overwrites the state of the other. A lock file
  next to `state.json`.
- **[B] `last_run` does not move if there are no posts** (`run_once` exits early) →
  `catch_up` runs an empty run on every restart after a quiet day. Treat "nothing to
  post" as a successful run and write `last_run`.
- **[B] Partial publishing.** Part 2 of 3 was not sent → the state did not move → the
  next run posts part 1 again. Retry sending each part (FLOOD_WAIT/network) before giving
  up; on a final failure — a notification.
- **[B] Rebuilding while the daemon runs.** Windows keeps the `.exe` locked —
  `cargo build --release` fails. Either stop the task before the build, or copy the
  binary to a separate folder and point schtasks to it.
- **[C] Dates in the local zone.** The title "Digest for DD.MM.YYYY" and `{{now}}` are
  calculated in UTC; an evening run in `America/New_York` gets tomorrow's date. Use
  `schedule.timezone`. Also move the strings "Digest" and "In short" to
  `[prompts]`/the config — now they are hard-coded in `digest.rs`.
- **[C] The 4096 limit is counted in UTF-16 code units,** but `chars().count()` counts
  scalar values: emoji = 2 units. The margin of 196 characters covers a normal digest,
  but with many emoji it may not be enough — count with `encode_utf16().count()`.

### Tests

There was no test plan at all; now there are 6 unit tests for `normalize_phone`,
`escape` and splitting. Without the network we can and should cover:

- `Scheduler::missed_since` / `next_after` — time zone border, daylight saving change,
  `last_run = None`;
- `State`: `advance` does not move back, `load` on broken JSON gives a clear error,
  `save` is atomic (tmp + rename);
- `parse_digest`: `stop_reason = "refusal"` and `"max_tokens"`, empty `content`,
  invalid JSON from the model, several `text` blocks;
- `render_html`: empty `tldr`, a topic without sources, escaping in `title`;
- the album merging and stop logic of `fetch_channel` — move it to a pure function over
  `Vec<(id, date, grouped_id, text)>`, so it can run without Telegram.

### Status on 2026-09-15

Steps 1–9 are written, `cargo fmt && cargo clippy --all-targets && cargo test` is clean
(49 unit tests). Done in Step 9: a database instead of `state.json` (`db.rs`), a run with
a delivery journal and recovery (`run.rs`, `telegram/publish.rs`), a Telegram folder as
the channel list with validation (`telegram/folder.rs`), commands in Saved Messages
(`telegram/commands.rs`), cooperative shutdown (`cancel.rs`), Dockerfile and compose.
Closed in code from Step 8: private channels, the cursor by the highest id, catch-up of
a single channel, a safety limit instead of a cut, validation of `sources[].url`, the
empty digest, `usage` in the logs and `/status`, notifications to Saved Messages,
`last_run` on an empty run, escaping the quote, the UTF-16 limit, dates and titles in
the time zone from the config, links from text entities, backoff on FLOOD_WAIT while
collecting and while sending, detection of a revoked session, partial publishing, a
lock instead of a lock file, logs to stdout, the working folder (volume `/data`).

Added in the second pass: reposts are marked with their source (step 4), map-reduce for
a large input (step 5), channel titles and usernames are read again before every run
with one request — with or without a folder, long command replies and notifications are
split by the message limit, a peer type check when looking up a channel by id and in
`/add`, removing the folder exclusion on `/add`. Map-reduce splits the input by channel,
merges the summaries in several rounds if they do not fit in one request, and checks the
whole final prompt: if it does not fit, the run fails with a clear error and the cursors
stay in place.

Not done: edited posts (on purpose, Step 8 [C]).

**None of this was run live**: the machine has neither `login` nor a container. The
verification (the list above and 9.7) is done only up to item 2 (`cargo build`). The
folder is not under git.

### Review against the plan — 2026-09-16

The checkout had no git history: the current tree was checked, not a diff. The
decisions of Step 9 have priority. These differences were fixed:

- **[A], 9.5:** `/run` no longer blocks reading commands; `/status` is available,
  state changes are refused through the shared `try_lock`, without a queue.
- **[A], 9.1:** a normal shutdown waits for both loops and the command handlers;
  sending the current part and writing the delivery are not dropped when the second
  loop exits. Reading a channel and waiting for a retry are stopped by the token;
  `/last` stops between parts.
- **[A], 9.3:** the start always checks for an undelivered run, whatever the cron
  settings are. The auto-retry window is checked before each attempt, including a retry
  after FLOOD_WAIT; `/resume` explicitly allows going past the window.
- **[A], CLI:** `preview` and `run --dry-run` sync the folder with a copy of the channels
  in memory and do not reset the cursors of the running daemon.
- **[A], Step 6:** long lines, titles and link labels are split within the UTF-16 limit,
  keeping the text, the HTML tags and the entities.
- **[B], 9.2:** an empty model answer keeps the post count and usage; damaged cursors
  give an error instead of a silent reset.
- **[A], 9.5:** a revoked session during the folder sync or a command reply goes out as
  a fatal error.

Checks: `cargo fmt --check`, `cargo clippy --all-targets --locked`,
`cargo test --locked` — 66 tests. Regression tests cover stopping both loops, the retry
window, preview isolation, the empty answer, damaged cursors, long HTML and command
locks. The network scenarios of 9.7 need a live check with Telegram/Docker; unit tests
do not prove server-side deduplication.

The origin of reposts (Step 4) and map-reduce (Step 5) have been done since then — see
"Status on 2026-09-15" above. Edits of posts that were already read are still not
supported, on purpose.


---

## Step 9. Docker, the database and managing channels from Telegram (decided 2026-09-15, revised after review)

### Decisions

| Question | Decision |
|---|---|
| Where the service lives | A Docker container, `restart: unless-stopped`, one `serve` process |
| Storage | SQLite through **`libsql`** — it is already in the dependency tree (`grammers-session` → `libsql 0.9.30`), we do not pull a second SQLite into the binary |
| What is in the database | channels (with `peer_id`/`access_hash`), the `last_msg_id` cursor per channel, runs and **digest parts with the fact of delivery** |
| What is in `config.toml` | secrets, cron, prompts, model, limits — what is changed at deploy time |
| Channel management | **a Telegram folder built by hand** as the source of the list + **commands in Saved Messages** of the same account |
| Bot | not needed: a bot does not read channels, and the account itself gives a UI for one owner. It can be added later as a second client on the same database, if other users appear |
| CLI | **only** `login` and the debug commands `preview` / `run --dry-run`. The CLI has no channel management commands |

Step 9 is done in code (see "Status on 2026-09-15" above): `state.json` is removed,
`db.rs`, `run.rs`, `cancel.rs`, `telegram/folder.rs`, `telegram/commands.rs`,
`Dockerfile` and `compose.yaml` were added. The Step 8 items marked "closed by design"
are now closed in code, but **not checked live**: each one is checked by a
verification item in 9.7, and that needs a running Telegram.

### 9.1 Docker

```
Dockerfile        # multi-stage: rust:1-bookworm → debian:bookworm-slim
compose.yaml      # volume /data, restart: unless-stopped, stop_grace_period: 30s
```

- All working files are in the `/data` volume: `config.toml`, `tg.session`, `digest.db`.
  The paths in the config are absolute (`session_file = "/data/tg.session"`), the CLI
  default is `--config /data/config.toml`. This closes the Step 8 item about the
  working folder by design.
- `rustls` is already there → the image does not need OpenSSL. We do not use
  Alpine/musl: `libsql` compiles SQLite from C, and with musl that causes extra
  trouble. `debian:bookworm-slim` + `ca-certificates`.
- **`login` is interactive, once**: `docker compose run --rm tg-digest login`
  (needs `-it`, stdin for the code and the 2FA password). After that the session is in
  the volume.
- **Logs go to stdout**, Docker collects them. The "logs to a file" item from Step 7 is
  dropped.
- `catch_up = true` is the normal mode: a container restart ≡ "the PC was asleep".

#### Shutdown (SIGTERM / Ctrl+C)

Rule: **anything not yet saved can be stopped; anything half saved must not be lost.**
The only operation that must not be stopped is sending one part of the digest
(seconds). Everything else is either idempotent or can be recovered from the database.

- Signal → `CancellationToken` (`tokio_util`). `select!` at the top level **does not
  wrap the run**: the run gets the token and checks it itself at checkpoints —
  between channels while collecting, before the LLM call, between parts while
  publishing.
- Post collection stopped → nothing is saved, the run is marked `error: cancelled`,
  the cursors are not touched. The next slot collects again.
- **The LLM request can be stopped** (`select!` on the token around the HTTP request):
  the tokens are wasted, but the database has nothing, a retry is safe. We cannot wait
  for the end — with `effort = "high"` the answer takes minutes, no sensible
  `stop_grace_period` covers that.
- Publishing: the current part is finished and saved in the database, then stop. The
  run stays in `publishing` status and is finished on the next trigger (9.3) with the
  same `random_id` — with SIGKILL in the middle of a send a duplicate is possible only if
  Telegram has already forgotten the `random_id` (9.3, "Expiry").
- `stop_grace_period: 30s` in compose. SIGKILL after it ≡ a container crash — the
  recovery scenario is the same as in 9.3, nothing special.
- Ctrl+C and SIGTERM (`tokio::signal::unix`, under `#[cfg(unix)]`) are the same token.
  A second signal means exit at once.

### 9.2 Database (`src/db.rs`)

```sql
CREATE TABLE channels (
  id          INTEGER PRIMARY KEY,          -- channel peer id (positive, without -100)
  access_hash INTEGER NOT NULL,
  username    TEXT,                          -- NULL for private channels
  title       TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1,
  last_msg_id INTEGER NOT NULL DEFAULT 0,    -- 0 = not read yet, window by lookback_hours
  last_error  TEXT,                          -- error of the last collection, NULL if ok
  added_at    TEXT NOT NULL
);

CREATE TABLE runs (
  id            INTEGER PRIMARY KEY,
  started_at    TEXT NOT NULL,
  finished_at   TEXT,
  status        TEXT NOT NULL,   -- collecting | summarized | publishing | ok | empty | error
  trigger       TEXT NOT NULL,   -- cron | catch_up | manual
  posts         INTEGER, topics INTEGER,
  input_tokens  INTEGER, output_tokens INTEGER,
  error         TEXT,
  cursors       TEXT,            -- JSON {channel_id: max_id_seen}: applied when status → ok | empty
  first_send_at TEXT,            -- time of the FIRST send attempt: expiry is counted from it (9.3)
  dest_label    TEXT,            -- "me" | "@channel" from the config at run time, for /status
  dest_peer_id  INTEGER,         -- the resolved destination peer: recovery sends HERE,
  dest_access_hash INTEGER       -- not to what is in the config now
);

CREATE TABLE run_chunks (
  run_id      INTEGER NOT NULL REFERENCES runs(id),
  idx         INTEGER NOT NULL,
  html        TEXT NOT NULL,
  random_id    INTEGER NOT NULL, -- random_id for messages.sendMessage; one per part, forever
  delivered_at TEXT,             -- the fact of delivery. NULL = not confirmed. THIS is the completion rule
  sent_msg_id  INTEGER,          -- message id, if Telegram returned it; metadata only
  PRIMARY KEY (run_id, idx)
);
```

- `state.json` and `src/state.rs` **are removed**. `last_run` for the scheduler =
  `MAX(started_at) FROM runs WHERE status IN ('ok','empty')`. The Step 8 item "`last_run`
  does not move on an empty run" is closed by design with the `empty` status.
- **Cursors move in one transaction with the `ok` or `empty` status.** `ok` — after the
  last part is delivered. `empty` — when after collecting and filtering there are no
  posts **or** the model returned `topics = []`: there is nothing to post, but the posts
  were read, and the cursors of the **successfully collected** channels are applied in
  the same transaction (channels excluded by the safety limit or an error are not
  touched). Without this, short and "unimportant" posts would be read again on every
  run. Until the change the cursors are in `runs.cursors` and do not affect selection. A
  failed publish does not "eat" posts (the rule from AGENTS.md).
- The channel cursor = **the highest id seen while collecting**, regardless of the length
  filter and album merging (the album bug from Step 8).
- `runs.*_tokens` and `/status` close by design the Step 8 items about `usage` and about
  "we learn a week later".
- `peer_id + access_hash` → selection by `PeerRef` without `resolve_username`. This
  gives **private channels** (item [A] from Step 8). Post link:
  `https://t.me/{username}/{id}` for public channels, `https://t.me/c/{peer_id}/{id}`
  for private ones.
- Migrations: a `schema_version` table, migrations are `&[&str]` in the code, applied on
  start.
- `[[channels]]` in the TOML is **only a seed**: with an empty table we import them and
  write it to the log, after that the truth is in the database and the folder. Say so in
  `config.example.toml`.

### 9.3 Collecting and publishing: limits and recovery

#### Collection limits — the channel cursor, not a global time

- A channel with `last_msg_id > 0`: we read **all** messages down to `id <= last_msg_id`.
  The date does not matter. A channel that failed for three days while the others worked
  gives all three days when it works again — the global `last_run` is **not used** for
  the limit (the idea `max(lookback, now − last_run)` from Step 8 is cancelled).
- A channel with `last_msg_id = 0` (new or reset): the window `now − lookback_hours`.
- **Turning a channel on again** (`enabled` 0 → 1 through the folder or `/add`) resets the
  cursor to 0. Otherwise a channel turned off for a month would bring a month of posts
  when it comes back.
- `max_posts_per_channel` is **a safety limit, not a cut**. Default 1000. If we hit it
  before reaching the cursor/window: the channel is **excluded** from this run, the cursor
  **does not move**, `channels.last_error` = "limit of N exceeded, raise
  `max_posts_per_channel` or reset the channel with `/reset`", and the same goes into a
  notification. The digest for the other channels is posted. No posts are silently lost
  with any limit value.
- A channel read error (FLOOD_WAIT after backoff, network) — the same: exclude it, do not
  touch the cursor, `last_error`, notify. The next run reads from the cursor.

#### Publishing — a delivery journal based on `random_id`

Telegram itself gives idempotency: `messages.sendMessage` accepts `random_id` "to prevent
sending the same message twice" — a retry with the same `random_id` within the server
dedup window does not create a second message. The length of this window is **not
documented**, so this lowers the risk but is not a guarantee; the remaining risk of a
duplicate is limited by the expiry policy below. `random_id` is generated **once, when
the part is written to the journal**, and is reused in all retries, including retries
after a restart.
No text comparison: checking "the last N messages" was dropped — a part could be outside
the window, and an old identical digest could give a false match.

Implementation: the high-level `send_message` in grammers does not accept `random_id` —
send through `client.invoke(messages::SendMessage { random_id, peer, message,
entities, no_webpage: true, .. })`, and get the markup from the same HTML parser
(`grammers_client::parsers::parse_html_message` — check against docs.rs 0.10).

1. The LLM answer is received → in one transaction: `runs.status = summarized`, `posts`,
   `topics`, `*_tokens`, `cursors`, **the resolved destination** (`dest_label`,
   `dest_peer_id`, `dest_access_hash` — we resolve `destination` from the config exactly
   here) and **all parts** in `run_chunks` with `random_id`, `delivered_at = NULL`.
   From this moment neither the digest nor the destination **is calculated again** —
   changing `destination` in the config and restarting does not send the rest to another
   chat.
2. `status = publishing`. Parts with `delivered_at IS NULL` are sent in order to
   `dest_peer_id`. We treat the result of `invoke` like this:
   - **success** → `delivered_at = now`, `sent_msg_id` = the id from the answer, if it is
     there (`updateShortSentMessage.id` / `updateNewChannelMessage`; for some peers the
     answer has no id — that is normal);
   - **`RANDOM_ID_DUPLICATE`** → Telegram **already has** a message with this
     `random_id`, it does not give the id. This is a **delivery**: `delivered_at = now`,
     `sent_msg_id = NULL`. Not an error, not a retry;
   - **unclear result** (timeout, broken connection, error without a code) → we save
     nothing, retry with **the same `random_id`** after backoff; the next answer falls
     into one of the two branches above;
   - **clear error** (`CHAT_WRITE_FORBIDDEN`, `PEER_ID_INVALID`, FLOOD_WAIT after
     backoff…) → the run stays `publishing`, a notification, the rule below.
   The shutdown token does not cancel sending a part.
3. **The completion rule is `delivered_at` for all parts**, not `sent_msg_id`.
   When it is met → in one transaction: `status = ok`, `finished_at`, apply `cursors`
   to `channels.last_msg_id`. `sent_msg_id` takes no part in the logic — only in
   `/status` for information.

After `MAX_ATTEMPTS` unclear results in a row the run stays in `publishing`, a
notification goes to Saved Messages, then the rules below apply.

**An unfinished run blocks a new collection.** The check `runs WHERE status IN
('summarized','publishing')` happens **on every trigger** — `serve` start, cron,
catch-up, `/run` — not only on start. If one is found, a new collection **does not
start**, whatever happens: the cursors have not moved, a second collection would give the
same posts and a second digest. The age of the found run decides what to do with it.

**Expiry.** `runs.first_send_at` is written before the **first** attempt to send the
first part and never changes after that. Telegram's deduplication by `random_id` is
limited in time, and **the length is not documented** — we do not rely on it.
`publish_retry_window_hours` (default **2**, `[telegram]`) is **a configurable retry
policy, not a safety limit**: inside the window the daemon retries delivery by itself
and accepts the remaining duplicate risk for the owner; outside it only the owner
retries. The value 2 is a compromise between "cover retries inside a run and a container
restart" and "do not retry after the server was down for the weekend"; nothing but
common sense supports it, and a duplicate inside the window **is possible** if
Telegram's window is shorter.
- `now − first_send_at ≤ window` → we finish the delivery automatically (steps 2–3 with
  the same `random_id` and `dest_peer_id`). `/run` answers "there is an undelivered run
  #N, retrying the delivery" and does only this.
- `now − first_send_at > window` → **there is no auto-retry**. The run stays
  `publishing`, a notification goes to Saved Messages (once, not on every trigger):
  "run #N not delivered since <first_send_at>, a retry is not guaranteed to be safe:
  `/resume N` — deliver the undelivered parts (a duplicate is possible if Telegram has
  already forgotten the `random_id`), `/abandon N` — cancel". The daemon waits for the
  owner to decide; meanwhile cron skips slots with a `warn!`.
- `first_send_at IS NULL` (failed between `summarized` and the first send) — the age is
  counted from `started_at`, the rule is the same.

Manual exits — both under the lock (9.5):
- `/resume <run_id>` — the owner accepts the risk of a duplicate; delivery uses the saved
  `random_id` (the best chance of deduplication if the time has not run out yet),
  `first_send_at` does not change — a second expiry will not happen, but it is not
  needed: after `/resume` there is either `ok` or a clear error and a new notification.
- `/abandon <run_id>` — `status = error: abandoned`, the cursors **are not applied**
  (the delivered parts stay in the chat, the posts go into the next digest — the owner
  decides which is worse).

There is no automatic "give up"; there is no automatic "take the risk" either.

A run in `collecting` status at start (failed before the LLM answer) becomes `error:
interrupted`, the cursors are not touched, it blocks nothing.

The Step 8 item "partial publishing → duplicates" is closed by design with this journal
**within the retry window**; outside it a duplicate stays a deliberate decision of the
owner through `/resume`. Check — 9.7 items 17, 20–22, 26.

### 9.4 The channel list = a Telegram folder built by hand

```toml
[telegram]
folder = "Digest"     # name of the folder (dialog filter) in the account
```

A Telegram folder is not only `include_peers`. A `dialogFilter` has flags for automatic
inclusion (`contacts`, `non_contacts`, `groups`, `broadcasts`, `bots`), exclusion
(`exclude_muted`, `exclude_read`, `exclude_archived`), `pinned_peers`, `exclude_peers`;
there are also `dialogFilterChatlist` (shared folders) and `dialogFilterDefault`.
An "all channels" folder (`broadcasts = true`, `include_peers = []`) looks empty when
read naively — and would turn everything off.

Solution: **we support only a folder built by hand** and we check this.
- Sync (before a run and on `/sync`): `messages.getDialogFilters` (raw TL through
  `client.invoke`, check against docs.rs grammers-client 0.10 / grammers-tl-types),
  filter by `title`.
- **Validation**: if any automatic inclusion or exclusion flag is set → the sync
  **refuses**: a `warn!` + a message to Saved Messages "folder Digest has automatic
  rules (broadcasts), remove them — sync is turned off", the database stays as it is.
  `dialogFilterDefault` — the same. We accept `dialogFilterChatlist`: it has only
  explicit lists.
- Content = `pinned_peers ∪ include_peers` **minus** `exclude_peers`, only
  `InputPeerChannel`. The peers come with `access_hash` at once.
- In the database: in the folder → upsert, `enabled = 1` (0 → 1 = cursor reset, see
  9.3); in the database but not in the folder → `enabled = 0`. **We do not delete.**
- The folder does not exist → a `warn!` + a notification once, we work from the
  database. The folder is valid and empty → an explicit "turn everything off", a run
  with status `empty`.
- To add a channel, follow it and drag it into the folder on your phone. To remove it,
  drag it out of the folder. Private channels work out of the box.

### 9.5 Commands in Saved Messages (`src/telegram/commands.rs`)

The daemon listens to updates (`SenderPool.updates`; in 0.10 this is a separate field of
the pool — check signatures against docs.rs) and reacts **only** to messages with
`outgoing == true` where the chat is the account itself (`peer.id == me.id`). Nobody
except the account owner can write there, so no separate authorization is needed.

Channels in commands are named **by `@username` or by numeric `id`** — private channels
have no username. `/list` and the `/add` reply always show the `id`.

| Command | Action |
|---|---|
| `/add @channel` · `/add <t.me/+link>` | resolve (for an invite — `messages.importChatInvite`) → add to the **folder** (`messages.updateDialogFilter`) → sync. Reply: `title`, `id` |
| `/rm @channel` · `/rm <id>` | remove from the folder → sync (`enabled = 0`) |
| `/list` | `id`, title, `@username` or "private", on/off, cursor, `last_error` |
| `/reset <id>` | cursor to 0 (read the `lookback_hours` window again), clear `last_error` |
| `/run` | run now, `trigger = manual`. A run is already going → "already going" |
| `/status` | last run: time, status, posts/topics, tokens, error; channels with `last_error` |
| `/last` | send the parts of the last run with status `ok` again to the **current** `destination`, with new `random_id` — this is a deliberate repeat, not a recovery |
| `/resume <run_id>` | deliver an old undelivered run and accept the risk of a duplicate (9.3) |
| `/abandon <run_id>` | release the lock of an undelivered run: `error: abandoned`, the cursors are not applied (9.3) |
| `/sync` | read the folder again (with the validation from 9.4) |
| `/help` | list of commands |

- The single source of truth is **the folder**. `/add` and `/rm` edit the folder, not
  the database, or the next sync undoes the edit. If `folder` is not set in the config,
  the commands edit the database directly.
- The reply is a message in the same chat. Command errors go there too, not to the log.
- **One lock for everything that touches cursors and run status.**
  A `tokio::sync::Mutex<()>` is held for the whole run (collect → LLM → publish,
  including the final transaction) and is taken with `try_lock` — nobody waits in a
  queue. `/reset`, `/abandon`, `/sync`, `/add`, `/rm` (sync resets cursors on 0 → 1)
  and `/last` also run under it. It replaces the lock file from Step 8: there is one
  process, there is no second instance in the container.
  - The lock is taken (a run is going) → the command is **refused** at once: "run #N is
    going, try again later". We do not delay it: a delayed `/reset` that fires five
    minutes later is more surprising than a refusal. A cron slot that falls on a
    running run is skipped with a `warn!`.
  - There is an **unfinished run** (`summarized`/`publishing`, the lock is free) →
    `/reset`, `/sync`, `/add`, `/rm` are also refused: "there is an undelivered run #N —
    `/run` to deliver it or `/abandon N`". Such a run has cursors in `runs.cursors` that
    would overwrite any reset on delivery. Only `/run` (= deliver, if the run is not too
    old), `/resume` (deliver an old one) and `/abandon` are allowed.
  - `/resume` and `/abandon` apply **only to an unfinished run that is not going** —
    under the lock, so the publisher cannot bring it to `ok` at the same time.
  - `/list`, `/status`, `/help` are read-only, without the lock.
- Notifications to Saved Messages: a run error, a channel excluded by the safety limit or
  by a read error, a folder that failed validation. We already have the client, it costs
  nothing.
- The update loop and the scheduler are two tasks in one process, `Client` is cloned.
  A revoked session (`AUTH_KEY_UNREGISTERED`) breaks both → the process exits with a
  non-zero code, Docker restarts it, and the logs say what to do (`login`). A restart
  loop is acceptable here: you can see it in `docker ps`, but you cannot see a silent
  daemon.

### 9.6 What changes in the earlier steps

- **Step 1**: `+ src/db.rs`, `+ src/telegram/commands.rs`, `− src/state.rs`,
  `+ Dockerfile`, `+ compose.yaml`. Dependencies: `libsql = "0.9"` (the same as in
  grammers-session), `tokio-util` (`CancellationToken`). Update the layout table in
  `AGENTS.md` during the implementation.
- **Step 2**: `[[channels]]` → seed; `+ telegram.folder`; `+ db_file`;
  `+ telegram.publish_retry_window_hours` (default 2, see 9.3 "Expiry");
  `session_file` is absolute; `max_posts_per_channel` — default 1000, meaning "safety
  limit".
- **Step 4**: selection by `PeerRef` from the database; the limit is the channel cursor
  (9.3), the date only with cursor 0; full pagination down to the limit; the safety
  limit = excluding the channel without moving the cursor.
- **Step 6**: do not post an empty digest (`status = empty`); validate `sources[].url`
  against the set of collected posts; the `run_chunks` delivery journal.
- **Step 7**: `serve` = scheduler + update loop; **every trigger** starts with a check for
  an unfinished run (9.3), a new collection does not start if there is one; cooperative
  shutdown through the token; logs to stdout.
- **Step 8**, closed **by design** (checked in 9.7): the working folder, private
  channels, `last_run` on an empty run, the lock file, notifications, `usage`, partial
  publishing, `max_posts_per_channel`, catch-up of a single channel, rebuilding while the
  daemon runs (`compose up --build`). The other Step 8 items still apply.

### 9.7 Verification (addition to the list)

11. `docker compose run --rm tg-digest login` — the session appeared in the volume.
12. A `Digest` folder with two channels → `preview` inside the container shows both;
    drag one out → `/sync` → `/list` shows it as disabled with the same cursor;
    put it back → cursor 0.
13. A folder with "Channels" turned on (an automatic rule) → `/sync` refuses, the
    database did not change.
14. `/add <t.me/+invite>` → the private channel is in the folder and in `/list` with its
    `id`; `/rm <id>` → disabled.
15. `max_posts_per_channel = 5` on an active channel → the channel is excluded, the
    cursor did not move, a notification came, the digest for the other channels is
    posted.
16. `/run` → the digest came; `/status` shows the tokens; `/last` sent it again.
17. A digest of three parts, `docker kill` after the first one → after the start Saved
    Messages has exactly three parts, no duplicates; `runs.status = ok`, the cursors
    moved.
18. `docker stop` while waiting — the stop is instant; during an LLM request — within
    `stop_grace_period`, the run is `error: cancelled`, the cursors are in place.
19. Revoke the session in Telegram ("Terminate all other sessions") → the container
    exits with an error and a clear text, `docker ps` shows restarts.
20. **`random_id` idempotency**: send a part, "lose" the answer on purpose (cut the
    network after `invoke`), retry with the same `random_id` → the chat has one message,
    `delivered_at` is filled. Then `/last` (new `random_id`) → a second one.
21. **The lock on a trigger**: break the sending (for example, set `dest_peer_id` to a
    peer that does not exist), wait for `publishing` with all attempts used, **without
    restarting the daemon** wait for cron / send `/run` → the message "undelivered run
    #N", there is no new run in `runs`, the cursors are in place. `/abandon N` → the next
    `/run` collects the same posts.
22. **The destination is fixed**: three parts, `docker kill` after the first one, change
    `destination` in the config, start → parts 2–3 went to the **old** chat,
    `status = ok`; the next run already goes to the new one.
23. **An empty run moves the cursors**: a channel with only short posts
    (< `min_post_chars`) → `status = empty`, the channel cursor moved to the highest id,
    a second `/run` does not read them again. A channel excluded by the safety limit in
    the same run keeps its cursor.
24. **`RANDOM_ID_DUPLICATE` = delivery**: a part is delivered and confirmed; call
    `invoke` by hand with the same `random_id` again → Telegram answers
    `RANDOM_ID_DUPLICATE` (or returns the same message) → both branches give
    `delivered_at`, there is no second message in the chat, the run reaches `ok`, the
    next trigger is **not** blocked. Also: a chunk with `delivered_at` but
    `sent_msg_id = NULL` — `/status` shows it as delivered.
25. **Commands do not race with publishing**: while a run is going, `/reset <id>`,
    `/abandon`, `/sync`, `/add`, `/rm`, `/last` → "run #N is going", the database did
    not change; `/list` and `/status` answer. With an unfinished (not running) run,
    `/reset` → a refusal with a hint; `/abandon N` → the next `/reset` works, the cursor
    is 0 and is not overwritten, because run N is no longer delivered.
26. **A long downtime**: three parts, `docker kill` after the first one, in the database
    move `first_send_at` back by `publish_retry_window_hours + 1` h by hand, start →
    parts 2–3 are **not sent**, the run stayed `publishing`, one notification
    "`/resume N` / `/abandon N`"; the next cron slot gives a `warn!`, there is no second
    notification and no new run in `runs`. `/resume N` → parts 2–3 went out (with the
    saved `random_id`), `status = ok`, the cursors are applied. Repeat the scenario with
    `/abandon N` → `error: abandoned`, the cursors are in place, the next `/run`
    collects the same posts.

---

## Update 2026-10-30: one channel list, in the database

The Telegram folder, the `[[channels]]` seed in the config and the environment
overrides for secrets are removed:

- The channel list lives only in the database. It is managed with `/add` and `/rm`
  in Saved Messages; `/sync` is gone. There is no sync step at all.
- Before every run, titles and usernames are still read again with one
  `channels.getChannels` call (`telegram/channels.rs`). This never changes the list.
- All settings, the secrets too, come only from `config.toml` (`data/config.toml`
  in the container). Old keys `telegram.folder` and `[[channels]]` are ignored.

The text above about folders and the seed is history: it describes earlier steps.
