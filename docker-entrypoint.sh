#!/bin/sh
# The volume at /data keeps the host owner. The process runs as that owner, so
# the files on the host stay with the person who edits them. When /data belongs
# to root (a new volume), it is given to the digest user (uid 1000).
#
# First word selects the program (before setpriv). Every branch runs under the
# same uid as the volume, with HOME=/data, so CLI login files get the right owner.
set -eu

run_as_data_user() {
    if [ "$(id -u)" = 0 ]; then
        mkdir -p /data /data/tools /data/.local
        uid=$(stat -c %u /data)
        gid=$(stat -c %g /data)
        if [ "$uid" = 0 ]; then
            uid=1000
            gid=1000
        fi
        # Skip the big tool trees: only fix ownership of the folder itself, then
        # one recursive chown when the folder owner differs.
        for dir in /data/tools /data/.local; do
            if [ -d "$dir" ] && [ "$(stat -c %u "$dir")" != "$uid" ]; then
                chown -R "$uid:$gid" "$dir"
            fi
        done
        find /data \
            \( -path /data/tools -o -path /data/.local \) -prune -o \
            ! -user "$uid" -exec chown "$uid:$gid" {} +
        export HOME=/data
        exec setpriv --reuid="$uid" --regid="$gid" --clear-groups -- "$@"
    fi
    export HOME=/data
    exec "$@"
}

case "${1:-}" in
    agents)
        shift
        run_as_data_user /usr/local/bin/agents "$@"
        ;;
    exec)
        shift
        if [ "$#" -eq 0 ]; then
            echo "usage: docker compose run --rm -it tg-digest exec <command>..." >&2
            exit 2
        fi
        run_as_data_user "$@"
        ;;
    *)
        run_as_data_user /usr/local/bin/tg-digest "$@"
        ;;
esac
