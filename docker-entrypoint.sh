#!/bin/sh
set -eu
umask 077

# Supervisor can restore app-owned files as root:0644. Normalize only this
# private volume, while root owns its inodes, before dropping privileges. This
# works without FOWNER and never follows links or crosses nested mounts.
if [ "$(id -u)" -eq 0 ]; then
    if [ -L /data ] || [ ! -d /data ]; then
        echo 'Private data must be a real directory' >&2
        exit 1
    fi
    if ! awk '$5 ~ /^\/data\// { exit 1 }' /proc/self/mountinfo; then
        echo 'Private data contains a nested mount' >&2
        exit 1
    fi
    # Internal hard links can remain after an interrupted atomic publication.
    # Count inode aliases without exposing filenames; any unseen alias lies
    # outside this volume. A failed walk must abort before changing metadata.
    if ! entries=$(find -P /data -xdev ! -type d -printf '%y %D:%i %n\n'); then
        echo 'Private data could not be inspected' >&2
        exit 1
    fi
    if ! printf '%s\n' "$entries" | awk '
        NF {
            if ($1 != "f" && $1 != "l") exit 1
            seen[$2]++; links[$2] = $3
        }
        END { for (inode in seen) if (seen[inode] != links[inode]) exit 1 }
    '; then
        echo 'Private data contains an external hard link or unsupported file type' >&2
        exit 1
    fi
    unset entries
    find -P /data -xdev ! -type l -exec chown --no-dereference root:root -- {} +
    find -P /data -xdev -type d -exec chmod 0700 -- {} +
    find -P /data -xdev -type f -exec chmod 0600 -- {} +
    find -P /data -xdev -exec chown --no-dereference closet:closet -- {} +
    exec gosu closet "$0" "$@"
fi
umask 077

if [ -f /data/options.json ]; then
    set -- --options /data/options.json "$@"
fi
if [ "${CLOSET_ENABLE_INGRESS:-false}" = true ]; then
    export CLOSET_INGRESS_BIND=0.0.0.0:8099
    export CLOSET_INGRESS_PEER=172.30.32.2
fi

exec closet "$@"
