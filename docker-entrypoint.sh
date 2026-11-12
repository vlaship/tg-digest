#!/bin/sh
# The volume at /data keeps the host owner. The process runs as that owner, so
# the files on the host stay with the person who edits them. When /data belongs
# to root (a new volume), it is given to the digest user (uid 1000).
set -eu
if [ "$(id -u)" = 0 ]; then
    mkdir -p /data
    uid=$(stat -c %u /data)
    gid=$(stat -c %g /data)
    if [ "$uid" = 0 ]; then
        uid=1000
        gid=1000
    fi
    # Only files with another owner are changed, for example the ones an older
    # image wrote as root.
    find /data ! -user "$uid" -exec chown "$uid:$gid" {} +
    exec setpriv --reuid="$uid" --regid="$gid" --clear-groups -- \
        /usr/local/bin/tg-digest "$@"
fi
exec /usr/local/bin/tg-digest "$@"
