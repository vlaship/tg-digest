# tg-digest

tg-digest reads a list of Telegram channels, makes a digest from them with Claude,
and posts the digest back to Telegram on a schedule.

- **Reading.** It uses MTProto with your personal account (`grammers`). So it can
  read any channel you follow, including private channels from an invite link.
- **Summary.** It uses the Anthropic Messages API. Posts are grouped by topic. If
  several channels write about the same event, they become one item with a list
  of sources.
- **Publishing.** The same account posts the digest: to Saved Messages or to your
  own channel.
- **Schedule.** A cron expression in the config. You do not need a separate OS
  scheduler.
- **Channel list.** A Telegram folder that you fill by hand on your phone. You
  control the service with commands in Saved Messages (`/run`, `/list`, `/status`).

## Run in Docker (the normal way)

Two things live outside the image:

- **`.env`** next to `compose.yaml` — the secrets (`TG_API_ID`, `TG_API_HASH`,
  `TG_PHONE`, `ANTHROPIC_API_KEY`). Compose passes them to the container.
- **`data/`** — a host folder mounted as `/data`: `config.toml` (schedule, prompts,
  limits), `tg.session` and the database `digest.db`. It survives image updates
  and `docker compose down`. Back it up, and keep it private: `tg.session` gives
  full access to your Telegram account.

Both are in `.gitignore`.

```powershell
# Do not overwrite existing files: they already have your secrets and state.
if (-not (Test-Path .env)) { Copy-Item .env.example .env }   # then fill in .env
New-Item -ItemType Directory -Force data | Out-Null
if (-not (Test-Path data\config.toml)) { Copy-Item config.example.toml data\config.toml }

docker compose build                          # build the image first, or login runs in the old one
docker compose run --rm -it tg-digest login   # one-time login: the code, and the password if you use 2FA
docker compose up -d
docker compose logs -f
```

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
digests. `preview` and `run --dry-run` use a copy of the channel list in memory:
in these commands the folder sync does not change the saved channels or cursors.

When the daemon starts, it always checks for an undelivered digest, whatever
`catch_up` and `run_on_start` say. A fresh one is delivered. For an old one it
waits for `/resume` or `/abandon`. Commands also work during a manual run, but
commands that change state are refused at that time. They are not queued.

## Settings

Fill in `config.toml`:

1. `telegram.api_id` / `api_hash` — get them at <https://my.telegram.org>;
2. `telegram.phone` — the phone number of the account that follows the channels;
3. `anthropic.api_key`;
4. `telegram.folder` — the name of the Telegram folder with the channel list.

With Docker, put items 1–3 into `.env` instead (see `.env.example`): `TG_API_ID`,
`TG_API_HASH`, `TG_PHONE` and `ANTHROPIC_API_KEY` override the values from the TOML,
so `config.toml` can keep its empty placeholders. An empty value in `.env` keeps
the one from `config.toml`.

### Channels

You build the folder by hand in the Telegram app. To add a channel, drag it into
the folder. To remove it, drag it out. A folder with automatic rules ("Channels",
"Unread") is refused: its content is calculated, and a sync would turn off all
channels at once.

`[[channels]]` in the config is only a seed for an empty database on the first
start.

## Commands in Saved Messages

| Command | What it does |
|---|---|
| `/add @channel`, `/add t.me/+link`, `/add id` | join the channel and put it in the folder |
| `/rm @channel` · `/rm id` | remove it from the folder |
| `/list` | channels: id, username, on/off, cursor, last error |
| `/reset id` | read the channel again from the `lookback_hours` window |
| `/run` | run now (or deliver an undelivered digest) |
| `/status` | last run: status, posts, topics, tokens, parts |
| `/last` | send the last digest again |
| `/resume N` · `/abandon N` | deliver or cancel an undelivered run |
| `/sync` | read the folder again |
| `/help` | list of commands |

Notifications also come there: a failed run, an excluded channel, a folder with
automatic rules, an undelivered digest.

## Security

`tg.session` gives full access to your Telegram account. It, the database and
`config.toml` with the keys are in `.gitignore` (the `data/` folder). Do not
commit them and do not copy them anywhere.

## Cost

A rough guide: 20 channels × ~30 posts ≈ 120k input tokens, about **$0.6–0.8**
per run on `claude-opus-5`. That is about $20 a month with one run a day. You can
change the model and `effort` in the config without a rebuild. The real usage is
in `/status` and in the logs.
