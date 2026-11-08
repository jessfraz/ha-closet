#!/bin/sh
set -eu

if [ "$(id -u)" -eq 0 ]; then
    chown closet:closet /data
    if [ -f /data/options.json ]; then
        chown closet:closet /data/options.json
    fi
    for reader_file in /data/reader-secrets.json /data/reader-ca.crt; do
        if [ -f "$reader_file" ] && [ ! -L "$reader_file" ]; then
            chown closet:closet "$reader_file"
        fi
    done
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
