# Improvements for tg-digest

These items are implemented. The notes below say why they were needed.

The delivery journal, the per-theme schedules, the owner-only bot, and the “do not move read marks until the digest is delivered” rule are in good shape. The items below are the gaps that can lose a run, a login, or a correct digest.

## 1. Photo downloads can kill the container

A photo is read fully into memory, and only then dropped if it is over 10 MB. One topic can keep up to 10 photos. `compose.yaml` caps the container at 128 MB so a memory spike does not hang the host.

A normal album, or one oversized file, can cross that cap. The kernel stops the process in the middle of a run. Docker starts it again, and a half-sent digest has to be recovered.

Stop reading a photo once it passes the limit, and write each file to the photos folder before downloading the next. Raise `mem_limit` only if a real digest still does not fit after that.

## 2. Commands share one database connection with the open run

Each bot command runs as its own task. `/add`, `/rm`, `/reset`, `/skip`, and `/last` wait for the run lock. `/model`, `/models`, `/list`, and `/status` do not. They all use the one `libsql` connection that `save_summary` and `complete_run` already have a transaction on.

A read or a model switch during that transaction joins it. If the run then rolls back, the switch rolls back with it. Two statements on that handle can also fail with “database is locked”.

Run database work on one task at a time (the same lock, or a separate mutex around `Db`). The README already says state-changing commands wait; `/model` and `/fallback` should be in that set too.

## 3. `preview` and `run --dry-run` use the live Telegram session

Those commands copy the database, so they do not move read marks. They still open `tg.session`. Telegram allows one connection for that key. A second one, for example a local preview while the container is up, comes back as `AUTH_KEY_DUPLICATED` and the login is dead until `login` is run again.

Say this next to the “two processes” note in the README. Refuse to connect when the daemon is already running on that session file.

## 4. An existing database does not pick up new columns

`CREATE TABLE IF NOT EXISTS` does not alter a table that is already there. The only migration is the theme column. `row_to_run` then reads `SELECT *` by column number, and the comment says new columns must stay last.

The next column on a volume that is already in use will make runs fail to load, or will map fields onto the wrong values. Read marks and undelivered parts sit in that file.

On open, add any missing column with `ALTER TABLE`, and read run rows by name. Keep the “replace the database” note only for a real incompatible change.

## 5. Docs disagree with the code

- The README opens with “summarizes them with Claude”, and the cost section prices `claude-opus-5`. The example config defaults to Gemini, and there are four providers. Describe the model as whatever `model` is set to, and mark the price as one example.
- The README says pull requests build the image and do not publish it. `.github/workflows/docker.yml` does not run on pull requests. It publishes on a push to `main`, and other branches only when someone starts the workflow by hand.

## 6. CI and the image

The cargo job installs unpinned `stable` and treats every clippy warning as an error. A new lint on the next toolchain update fails the required check with no code change. Pin the Rust version (a `rust-toolchain.toml` file is enough).

Nothing checks the lockfile for known vulnerable crates. The build depends on a pinned pre-release (`glass_pumpkin` 2.0.0-rc0). Add a lockfile advisory check and keep that pin.

The image runs as root, from floating `rust:1-bookworm` and `debian:bookworm-slim` tags. The volume holds the Telegram session and the API keys. Run as a fixed user that can write `/data`, and pin the base image digests the way `Cargo.lock` pins crates.
