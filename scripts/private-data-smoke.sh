#!/usr/bin/env bash
set -euo pipefail
image=${1:?image required}
scratch=$(mktemp -d)
container="private-data-smoke-${RANDOM}-${RANDOM}"
volume="$container-data"
cleanup() {
    docker rm --force "$container" >/dev/null 2>&1 || true
    docker volume rm "$volume" >/dev/null 2>&1 || true
    rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker volume create "$volume" >/dev/null
helper() {
    docker run --rm --network none --entrypoint /bin/sh \
        --mount "type=volume,source=$volume,target=/volume" "$image" -ec "$1"
}
refuse() {
    docker start --attach "$container" >"$scratch/output" 2>&1 || true
    test "$(docker inspect "$container" --format '{{.State.ExitCode}}')" != 0
    docker rm "$container" >/dev/null
}
outside_unchanged() {
    test "$(helper 'stat -c %a:%u:%g /volume/sentinel')" = 644:0:0
}

# A hard link outside the private subpath must be rejected before changing the
# shared inode. Both names live on one filesystem, as in a real hard-link attack.
helper 'mkdir /volume/private; printf "Fictional outside state\n" > /volume/sentinel; chmod 0644 /volume/sentinel; ln /volume/sentinel /volume/private/external-alias'
docker create --name "$container" --network none --cap-drop FOWNER \
    --mount "type=volume,source=$volume,target=/data,volume-subpath=private" \
    "$image" --version >/dev/null
refuse
outside_unchanged
helper 'rm /volume/private/external-alias; printf "Fictional private state\n" > /volume/private/state; ln /volume/private/state /volume/private/atomic-remnant; ln -s /outside/sentinel /volume/private/external-symlink'

# Internal publication links remain valid, but external symlink targets must not
# be followed. Read-only outside storage makes accidental traversal observable.
# The second launch starts with10001-owned state, as an ordinary restart does.
for _ in 1 2; do
    docker run --rm --network none --cap-drop FOWNER \
        --mount "type=volume,source=$volume,target=/data,volume-subpath=private" \
        --mount "type=volume,source=$volume,target=/outside,readonly" "$image" --version >/dev/null
    test "$(helper 'stat -c %a:%u:%g /volume/private')" = 700:10001:10001
    test "$(helper 'stat -c %a:%u:%g /volume/private/state')" = 600:10001:10001
    test "$(helper 'stat -c %a:%u:%g /volume/private/atomic-remnant')" = 600:10001:10001
    outside_unchanged
done

# Refuse nested mounts before changing even the legitimate private files.
docker create --name "$container" --network none --cap-drop FOWNER \
    --mount "type=volume,source=$volume,target=/data,volume-subpath=private" \
    --mount "type=volume,source=$volume,target=/data/nested,readonly" \
    "$image" --version >/dev/null
refuse
test "$(helper 'stat -c %a:%u:%g /volume/private/state')" = 600:10001:10001
outside_unchanged

# The root of the owned volume is never allowed to be a symlink either.
docker create --name "$container" --network none --cap-drop FOWNER \
    --mount "type=volume,source=$volume,target=/outside,readonly" \
    --entrypoint /bin/sh "$image" -ec 'rmdir /data; ln -s /outside /data; exec /usr/local/bin/docker-entrypoint.sh --version' >/dev/null
refuse
outside_unchanged
printf '%s\n' 'Private-data boundary smoke passed: external hardlinks/mounts refused, internal publication links retained, external symlinks untouched, FOWNER not required on restore or ordinary restart.'
