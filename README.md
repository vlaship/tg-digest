# tg-digest

tg-digest reads a list of Telegram channels, makes a digest from them with the
model set in the config, and posts the digest back to Telegram on a schedule.

- **Reading.** It uses MTProto with your personal account (`grammers`). So it can
  read any channel you follow, including private channels from an invite link.
- **Summary.** It calls the model you set in `model` (Anthropic, Gemini,
  OpenRouter or OpenAI). Posts are grouped by topic. If several channels write
  about the same event, they become one item with a list of sources.
- **Bot.** You talk to your own Telegram bot: you write commands to it, and it
  sends you the replies, the digests and the notifications. It can also post the
  digest to your channel. Only your account may give it commands.
- **Schedule.** Each theme has its own cron in the config. Time zone and
  catch-up flags live under `[schedule]`. You do not need a separate OS
  scheduler.
- **Channel list.** Stored in the database. You add and remove channels with
  commands to the bot (`/add`, `/rm`), and control the service there too
  (`/run`, `/list`, `/status`).

## Run in Docker (the normal way)

Everything lives outside the image, in the host folder **`data/`**, which compose
mounts as `/data`:

- `config.toml` — all settings, including the secrets (API id and hash, phone,
  the keys of the model providers);
- `tg.session` — the Telegram login of your account, created by `login`;
- `bot.session` — the bot login, created on the first start;
- `digest.db` — the database.

It survives image updates and `docker compose down`. `data/` is in `.gitignore`
and `.dockerignore`. Back it up, and keep it private: it has your keys, and
`tg.session` gives full access to your Telegram account.

```powershell
New-Item -ItemType Directory -Force data | Out-Null
# Do not overwrite an existing config: it already has your secrets.
if (-not (Test-Path data\config.toml)) { Copy-Item config.example.toml data\config.toml }
# Now fill in data\config.toml (see "Settings" below).

docker compose build                          # build the image first, or login runs in the old one
docker compose run --rm -it tg-digest login   # one-time login: the code, and the password if you use 2FA
# No code comes? Use `login --qr`: scan the QR code in Telegram → Settings → Devices.
docker compose up -d
docker compose logs -f
```

Then open your bot in Telegram and send it `/start`. A bot can write to you only
after you wrote to it first; after that it remembers the chat.

`restart: unless-stopped` starts the container again after a reboot.
`catch_up = true` in the config runs a slot that was missed while the service
was down.

### Ready image

CI publishes the image to GitHub Container Registry:

- `ghcr.io/vlaship/tg-digest:latest` — the last build of `main`;
- `ghcr.io/vlaship/tg-digest:20261030-011220` — one exact build, by its UTC build
  time (`YYYYMMDD-HHmmss`). The same time is the version in `/status`, so you
  can see which image is running.

A push to `main` builds the image and publishes it. Pull requests do not run
the docker workflow. On any other branch it runs only when you start it by hand
(Actions → docker → Run workflow, or `gh workflow run docker`). A manual run
only builds, unless you tick "publish".

`compose.yaml` uses `latest`, so `docker compose pull` takes the new image.
For one exact build, put its tag in `image:`.

Two short forms are in the `Makefile`:

- `make update` — `docker compose pull`, `up -d`, then follow the logs;
- `make restart` — restart the container (it reads `data/config.toml` again),
  then follow the logs.

`Ctrl+C` stops following the logs, the container keeps running.

Stopping (`docker stop`) is safe. Collecting posts and the model request stop at
once. The current part of the digest is sent to the end. Anything not delivered
is sent after the next start.

## Local build

You need Rust with the MSVC linker:

```powershell
winget install --id Rustlang.Rustup --exact
winget install --id Microsoft.VisualStudio.2022.BuildTools --exact `
  --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

Restart the terminal, then:

```powershell
cargo build --release
Copy-Item config.example.toml config.toml
```

The default folder is for the container (`data_dir = "/data"`). When you run
locally, pass the config file and set `data_dir = "data"` in it. The database and
the two sessions are files in that folder; their names (`db_file`,
`session_file`) are optional and default to `digest.db`, `tg.session` and
`bot.session`:

```powershell
cargo run -- --config config.toml login          # one-time login
cargo run -- --config config.toml login --qr     # login by a QR code, if the code does not come
cargo run -- --config config.toml preview        # how many posts were collected, no model call
cargo run -- --config config.toml preview --theme dev --dump  # same, and write out/posts-dev.md (raw posts, no LLM)
cargo run -- --config config.toml run --dry-run  # digest in out/digest.html (out/digest-<tag>.html with several themes), nothing is posted; --theme dev for one theme
cargo run -- --config config.toml serve          # the daemon: schedule + commands
```

The CLI has no run that posts. Only the daemon posts: on the schedule or after
the `/run` command. Otherwise two processes on one database could post two
digests. Do not run `preview` or `run --dry-run` while the daemon is up. They
would open the same `tg.session`. Telegram allows one connection for that key.
A second one comes back as `AUTH_KEY_DUPLICATED` and the login is dead until
you run `login` again. Those commands refuse to connect when the daemon already
holds the session. `preview` and `run --dry-run` use a copy of the database in
memory: they do not change the saved channels or cursors. `preview --dump` (or
`--out path.md`) writes the collected posts as markdown for offline use; the
default file is `out/posts-<theme>.md`.

When the daemon starts, it always checks for an undelivered digest, whatever
`catch_up` and `run_on_start` say. A fresh one is delivered. For an old one it
waits for `/resume` or `/abandon`. A theme that had an undelivered digest does not
also get a new run on that start (`run_on_start` or `catch_up`); the other themes
do. Commands also work during a manual run, but commands that change state (and
`/last`) are refused at that time. They are not queued. That set is `/add`,
`/rm`, `/reset`, `/skip`, `/reread`, `/last`, `/model` and `/fallback`.
An undelivered digest of one theme blocks `/add`, `/rm`, `/reset`, `/skip` and
`/reread` only for that theme.

## Settings

Fill in `config.toml`:

1. `telegram.app.api_id` / `api_hash` — get them at <https://my.telegram.org>;
2. `telegram.app.phone` — the phone number of the account that follows the channels;
3. `telegram.bot.token` — create a bot with [@BotFather](https://t.me/BotFather): send it
   `/newbot`, choose a name and a username, and copy the token it gives you;
4. the models: `api_key` of each provider you use in `[providers.<provider>]`, then
   `model` (the main one) and `fallback` (where `/fallback` goes), both as
   `"provider/model id"`. The limits of each model come from the provider's own model
   list; a `[providers.<provider>.models."<model id>"]` table only changes them by hand.
   Providers: `anthropic`, `gemini` (key from <https://aistudio.google.com/apikey>),
   `openrouter` (key from <https://openrouter.ai/keys>), `openai` (key from
   <https://platform.openai.com/api-keys>; a ChatGPT subscription gives no API access).

`telegram.destination = "bot"` (the default) sends the digests to the chat with
the bot. To post to your channel instead, set `"@your_channel"` and make the bot
an admin of that channel with the right to post.

With Docker this file is `data/config.toml`. All settings come only from this
file, the secrets too.

### Channels

The channel list is not in the config: it is stored in the database. Add a
channel with `/add` to the bot — by `@username`, by a `t.me/name` link, by an invite link
(`t.me/+…`, the account joins it) or by a numeric id (a private channel the
account already follows). Remove it with `/rm`: the channel is turned off, not
deleted, so `/list` still shows it. Titles and usernames are read again before
every run, so a renamed channel keeps correct links.

### Themes

A theme is its own digest: its own channels, system prompt, title, cron and, if
you want, its own destination. The config needs at least one theme. You describe
themes as `[[themes]]` blocks (see `config.example.toml`: `world` for general
news and `dev` for programming channels, which also drops adverts for courses):

```toml
[[themes]]
tag         = "dev"
title       = "💻 Digest DEV {{date}} {{time}}"
cron        = "0 18 * * *"        # needed: this theme's schedule
destination = "@my_dev_channel"   # optional, else [telegram].destination
system      = """..."""
```

Each theme fires on its own cron. When two themes share a slot they run one
after another, in the order of the blocks, and each one posts its own digest.
They do not depend on each other: a channel can be in several themes (each theme
keeps its own cursor for it, so its news is in each digest), and an error in one
theme does not stop the others.

The tag names the theme in commands: `/add dev @channel`, `/run dev`. `/add`
without a tag uses the first theme. Channels from before themes go to the first
theme on the first start; an old database is updated by itself. Do not rename a
tag later: the channels are stored under it, and `/list` shows channels of a tag
that is not in the config as "not read".

## Commands to the bot

| Command | What it does |
|---|---|
| `/add @channel`, `/add t.me/name`, `/add t.me/+link`, `/add id` | add the channel; the digest starts from your read mark if you follow it, else only posts after adding go into it. Several channels at once: `/add @a @b @c` (spaces, commas or new lines; a new line may start with `/add` again). With a theme: `/add dev @a @b`; without one the first theme is used |
| `/rm @channel` · `/rm id` | turn the channel off (`@name`, `t.me/name` or id). Several at once: `/rm @a @b`, or one `/rm` per line in one message. `/rm dev @channel` when the channel is in several themes |
| `/list` | channels by theme, one message per theme: id, username, on/off, cursor, last error |
| `/reset id` | read the channel again from the `lookback_hours` window (`/reset dev id` for one theme) |
| `/skip` | move all channels to their newest message: older posts are skipped (`/skip dev` for one theme) |
| `/run` | run all themes now (or deliver an undelivered digest); `/run dev` runs one theme. After a digest in the chat the bot sends `/run` to tap |
| `/reread N` | read all channels again for the last N days (1–7) and run now; the last digest topics are not excluded. `/reread N dev` for one theme. Each theme keeps its window until it has read its posts: a theme whose run failed uses it on its next run |
| `/status` | last run of each theme: status, posts, topics, tokens, parts |
| `/last` | send the last digest of each theme again (`/last dev` for one) |
| `/model [provider/model]` | the models from the config with their limits, or switch to any model of a provider. Model ids are in monospace: one tap copies them |
| `/models provider [word]` | the models the provider has; the word filters the ids, `free` keeps the free ones |
| `/fallback` | switch to the next model in `fallback` and run now |
| `/resume N` · `/abandon N` | deliver or cancel an undelivered run |
| `/help` | list of commands |

Each digest goes to you (or to your channel, see `destination`).
A story from the last digest is not sent again, unless there is real news about it.
After a delivered digest the posts are marked as read in Telegram for your account
(`telegram.mark_read`), so the channels do not keep unread counters.
Notifications come only to you: a failed run, an excluded channel, an undelivered
digest.

## Security

`tg.session` gives full access to your Telegram account, and the bot token gives
full control of the bot. They, `bot.session`, the database and `config.toml` with
the keys are in `.gitignore` (the `data/` folder). Do not commit them and do not
copy them anywhere. The bot answers only your account and ignores everyone
else. The container process runs as user `digest` (uid 1000), which can write
`/data`. To stop strangers adding the bot to groups, send @BotFather
`/setjoingroups` → Disable.

## Cost

One example, not a price for every model: 20 channels × ~30 posts ≈ 120k input
tokens, about **$0.6–0.8** per run on `claude-opus-5`. That example is about $20
a month with one run a day. A cheaper model costs less. The real price is whatever
`model` is set to. You can change the model and `effort` in the config without a
rebuild. The real usage is in `/status` and in the logs.
