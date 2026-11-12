#!/bin/sh
# The volume at /data keeps the host owner. Root fixes that, then the process
# runs as the digest user (uid 1000), which can write the session and the database.
set -eu
if [ "$(id -u)" = 0 ]; then
    mkdir -p /data
    chown -R digest:digest /data
    exec setpriv --reuid=digest --regid=digest --init-groups -- \
        /usr/local/bin/tg-digest "$@"
fi
exec /usr/local/bin/tg-digest "$@"
