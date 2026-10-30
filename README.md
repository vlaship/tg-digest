# tg-digest

tg-digest reads a list of Telegram channels, makes a digest from them with Claude,
and posts the digest back to Telegram on a schedule.

- **Reading.** It uses MTProto with your personal account (`grammers`). So it can
  read any channel you follow, including private channels from an invite link.
- **Summary.** It uses the Anthropic Messages API. Posts are grouped by topic. If
  several channels write about the same event, they become one item with a list
  of sources.
- **Bot.** You talk to your own Telegram bot: you write commands to it, and it
  sends you the replies, the digests and the notifications. It can also post the
  digest to your channel. Only your account and the users in `bot.allowed_users`
  may give it commands.
- **Schedule.** A cron expression in the config. You do not need a separate OS
  scheduler.
- **Channel list.** Stored in the database. You add and remove channels with
  commands to the bot (`/add`, `/rm`), and control the service there too
  (`/run`, `/list`, `/status`).

## Run in Docker (the normal way)

Everything lives outside the image, in the host folder **`data/`**, which compose
mounts as `/data`:

- `config.toml` — all settings, including the secrets (API id and hash, phone,
  Anthropic key);
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
  time (`YYYYMMDD-HHmmss`);
- `ghcr.io/vlaship/tg-digest:1.2.3` and `:1.2` — from a `v1.2.3` git tag. A tag
  build gets only these version tags:

  ```bash
  git tag v1.2.3
  git push origin v1.2.3
  ```

Pull requests only build the image, they do not publish it. To build it by hand
without publishing, run the `docker` workflow from the Actions tab (or
`gh workflow run docker`); tick "publish" only if you want to push the result.

To use it instead of a local build, change `image:` in `compose.yaml` to one of
these tags and run `docker compose pull` instead of `docker compose build`.

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

The default paths are for the container (`/data/...`). When you run locally,
pass the config file and change `session_file` and `db_file` in it:

```powershell
cargo run -- --config config.toml login          # one-time login
cargo run -- --config config.toml preview        # how many posts were collected, no call to Claude
cargo run -- --config config.toml run --dry-run  # digest in out/digest.html, nothing is posted
cargo run -- --config config.toml serve          # the daemon: schedule + commands
```

The CLI has no run that posts. Only the daemon posts: on the schedule or after
the `/run` command. Otherwise two processes on one database could post two
digests. `preview` and `run --dry-run` use a copy of the database in memory:
they do not change the saved channels or cursors.

When the daemon starts, it always checks for an undelivered digest, whatever
`catch_up` and `run_on_start` say. A fresh one is delivered. For an old one it
waits for `/resume` or `/abandon`. Commands also work during a manual run, but
commands that change state are refused at that time. They are not queued.

## Settings

Fill in `config.toml`:

1. `telegram.api_id` / `api_hash` — get them at <https://my.telegram.org>;
2. `telegram.phone` — the phone number of the account that follows the channels;
3. `anthropic.api_key`, or `provider = "gemini"` and `gemini.api_key` (from https://aistudio.google.com/apikey);
4. `bot.token` — create a bot with [@BotFather](https://t.me/BotFather): send it
   `/newbot`, choose a name and a username, and copy the token it gives you.

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

## Commands to the bot

| Command | What it does |
|---|---|
| `/add @channel`, `/add t.me/name`, `/add t.me/+link`, `/add id` | add the channel; only posts after adding go into the digest |
| `/rm @channel` · `/rm id` | turn the channel off |
| `/list` | channels: id, username, on/off, cursor, last error |
| `/reset id` | read the channel again from the `lookback_hours` window |
| `/skip` | move all channels to their newest message: older posts are skipped |
| `/run` | run now (or deliver an undelivered digest) |
| `/status` | last run: status, posts, topics, tokens, parts |
| `/last` | send the last digest again |
| `/provider [name]` | show or switch the model provider (only the owner) |
| `/fallback` | switch to `fallback_provider` and run now (only the owner) |
| `/resume N` · `/abandon N` | deliver or cancel an undelivered run |
| `/stop` · `/start` | stop or start getting digests for yourself, the owner too (commands still work) |
| `/help` | list of commands |

Each digest goes to you and to every user in `bot.allowed_users` who has started
the bot, except those who sent `/stop` (you can send it too).
A story from the last digest is not sent again, unless there is real news about it.
After a delivered digest the posts are marked as read in Telegram for your account
(`telegram.mark_read`), so the channels do not keep unread counters.
Notifications come only to you: a failed run, an excluded channel, an undelivered
digest.

## Security

`tg.session` gives full access to your Telegram account, and the bot token gives
full control of the bot. They, `bot.session`, the database and `config.toml` with
the keys are in `.gitignore` (the `data/` folder). Do not commit them and do not
copy them anywhere. The bot answers only your account and the user ids in
`bot.allowed_users`, and ignores everyone else. To stop strangers adding the bot
to groups, send @BotFather `/setjoingroups` → Disable.

## Cost

A rough guide: 20 channels × ~30 posts ≈ 120k input tokens, about **$0.6–0.8**
per run on `claude-opus-5`. That is about $20 a month with one run a day. You can
change the model and `effort` in the config without a rebuild. The real usage is
in `/status` and in the logs.
