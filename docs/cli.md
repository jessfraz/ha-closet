# ha-closet

Validate a catalog locally, sync it to Closet, or read the app's inventory.
The CLI uses the same Rust types and validator as the service.

## Validate and sync

A `.toml` input loads sibling `brands.toml`, `categories.toml` and
`fittings.toml` files. See the [catalog format](catalog.md) and
[fictional example](../config/wardrobe.example/wardrobe.toml). Existing JSON
arrays are also supported.

```sh
ha-closet inventory validate --input wardrobe.toml
ha-closet inventory sync --input wardrobe.toml --dry-run
```

Validation is offline. It does not resolve a server URL, read a credential,
connect to Home Assistant, or write anything. This is the command to run in CI.
Every item needs a shop or purchase source and an explicit positive quantity.
TOML also requires brand, name, category, color, size, materials, photos and price.
Missing values fail validation; quantities are never defaulted.
Invalid input exits nonzero with safe field locations and error codes, without
printing item names or input values.

Sync sends the complete catalog, not a patch:

```sh
ha-closet inventory sync --input wardrobe.toml
```

The command previews the source, applies it with that exact revision and a
stable generated key, then reads every page back to verify the complete
projection. A stale revision fails; it never fetches a newer revision to force
a failed write through. For caller-owned retry control, pass both
`--if-match '"0"'` and `--idempotency-key catalog-revision-1` instead.

A sync creates or updates records by stable ID, archives absent records, and
restores records that reappear. It never permanently deletes inventory records.
An empty catalog needs `--allow-empty`. Use `--input -`, or omit `--input`, for
JSON stdin. JSON input and each TOML source file are limited to 1 MiB; paths
are never URLs. TOML needs a file path so its registries can be resolved.

### Local photos

With an HTTPS Home Assistant connection, sync also handles native photos. TOML
references local assets directly by SHA in ordered image galleries, with PNGs
in the sibling `wardrobe-images/` directory. It does not need an image manifest
or store deployment URLs. Local paths are fixed for TOML inputs.

JSON catalogs keep their existing `wardrobe-images.json` manifest, with SHA-named
PNGs in `wardrobe-images/`:

```json
{
  "version": 1,
  "images": [
    {
      "item_id": "example-shirt",
      "image_index": 0,
      "sha256": "<64 lowercase hexadecimal characters>"
    }
  ]
}
```

`image_index` selects an existing image reference, starting at zero. Its URL
is resolved in the outgoing snapshot only. Kind, provenance, crop and every
other catalog field stay unchanged. Files on disk are never rewritten. The
PNG filename is `<sha256>.png`. Use `--image-manifest PATH --images DIRECTORY`
for different paths. Stdin requires an explicit manifest path.

An optional `native_id` is a deployment hint, not source identity. Older
manifests can omit `image_index` when that hint identifies exactly one image
on the named item. Exact native URL aliases across purchases share the same
resolved image. If both fields are present, they must identify the same slot.
An optional `upload_name` must equal `closet-<sha256>.png`.

Validation checks mappings, hashes, the full decoded PNG and delivery encoding,
regular files, 512 assets at most, 10 MiB minus 1 KiB per source and 512 MiB
total. Unlisted files and symlinks fail. Keep local photos and their manifest
in private storage. URL-only references need no local file or manifest.

The same Rust pipeline prepares photos during offline validation and sync.
It preserves the aspect ratio, applies orientation once, normalizes supported
color profiles to sRGB, keeps transparency, and never crops or upscales. Deliveries have a maximum 2048-pixel
long edge and 2 MiB encoded size. If needed, the long edge steps down to 1792,
1536 or 1280; originals already below 1280 have a final 1024-pixel fallback.
JPEG quality stays at 92. Transparent photos use lossless PNG.
Corrupt files and unsupported color metadata fail validation instead of being
silently uploaded. No AI or external image service is involved.

Source SHA identities remain unchanged. A separate delivery SHA identifies the
actual uploaded bytes in `closet-v1-<delivery-sha>.jpg` or `.png`. Validation
reports source bytes, upload bytes, the policy version and a delivery
fingerprint. Codec versions are pinned and release CI checks the same golden
fingerprint on all three CLI platforms.

Sync reuses a native image only after exact-byte SHA verification. A missing
copy gets one multipart upload. A lost reply is reconciled through native
image/list and exact-byte readback, never a blind second POST. An unresolved
outcome returns `image_upload_pending`; rerunning can recover an existing
copy. If it remains absent, inspect the original operation before explicitly
resolving its durable claim. No uploads or cleanup happen during validation.

After successful inventory and full readback, cleanup removes only exact
`closet-v1-<sha256>.jpg` / `.png` objects and legacy `closet-<sha256>.png`
objects whose bytes match their name. Active records and
currently displayed Today references, including archived records and native
thumbnail aliases, protect shared images. Unrelated HA images are untouched.
A durable server fence blocks all inventory writers while admitted deletions
are pending, even after a crash or lease expiration. `cleanup_pending` means
rerun the same command to reconcile/resume those exact IDs. Failures before
cleanup never authorize a new deletion batch. If a changed source refers to an
image already admitted for deletion, `cleanup_source_conflict` stops before
cleanup. Keep local source bytes or use a live replacement reference before
resolving that interrupted operation.

The app, integration and CLI must be upgraded together for managed sync. An
older service without the fence endpoint fails before any image upload. The
owner token is kept under the user state directory; `--state-dir DIRECTORY`
can place it on persistent CI storage. Unresolved upload claims and admitted
deletions are also durable on the server, so a fresh CI worker cannot bypass
an uncertain write. No database transaction is held during a network call.

`--dry-run` performs only reads and preview requests. If native copies exist,
it reports the resolved source hash, inventory diff and cleanup count. Missing
copies report `would_upload` and defer the inventory diff: their random native
IDs do not exist yet. The cleanup count is also null until those images can
be resolved. Apply and dry-run never pretend the canonical source
hash is the resolved deployment hash. For TOML, offline `source_hash` and sync
`input_source_hash` describe the canonical authoring snapshot;
`resolved_source_hash` describes the fully materialized HTTP catalog. All
registries and asset files are checked again before catalog commit and cleanup.

## Connect

Install through the repository's Nix package:

```sh
nix run .#ha-closet -- --help
```

Choose one connection:

| Transport | Origin | Credential | API prefix |
| --- | --- | --- | --- |
| Native | `CLOSET_API_URL` | `CLOSET_API_TOKEN` | `/v1` |
| Home Assistant | `HOME_ASSISTANT_URL` | `HOME_ASSISTANT_TOKEN` | `/api/closet/v1` |

An origin is an HTTP(S) URL without a path, query, fragment, or embedded
credential. Use HTTPS outside a trusted local network. If both origins are
configured, select `--transport native` or `--transport home-assistant`.
An explicit selection never borrows the other transport's credential.

`--url` overrides the selected origin. `--token-file` reads an owner-only regular
file instead of the selected credential environment variable. Credentials are
never accepted as command-line arguments. The CLI does not log in, discover
credentials, fabricate HA user headers, or follow redirects.

The HA bridge supports catalog reads, sync, its cleanup fence and Today. Sync and generation
require an active HA administrator; the bridge derives the actor from the
authenticated HA user. Native writes require the owner credential. The
integration's read credential alone does not authorize a sync.

## Read the app

```sh
ha-closet system
ha-closet items list --limit 50
ha-closet items list --brand 'Example Label' --format table
ha-closet items list --source 'shop:example-shop'
ha-closet items get example-shirt
ha-closet --transport home-assistant items list
```

List filters are `--q`, `--brand`, `--category`, `--source`,
`--ready-to-wear true|false`, `--status` and `--area`.
Category filters include their dot-delimited descendants. Source keys are
`shop:<id>` or `purchase:<value>`; copy an exact key from the JSON
`facets.sources` list. A shop takes precedence over a purchase source.
Each invocation fetches one page. Apply filters on the first request, then pass
the returned `next_page` value with `--page-token` and no filters. The cursor
carries the applied query; `--limit` and `--format` may change. The items-list command never
drains pages or restarts an invalidated cursor. Full sync intentionally verifies
every active page.

JSON is the default, even in a terminal. Success is one newline-terminated
envelope with HTTP `status`, exact quoted `etag` or null, and typed `data`.
Offline validation uses null status/ETag because no HTTP request occurred.
`--format table` is available for `items list` and `today get` only and does not
change the request or fetch additional information. Response bodies are limited
to 8 MiB. Successful output contains the requested inventory and should be kept
private.

## Tracking

The same connection and administrator authentication support RFID operations.
Catalog metadata stays in the catalog; tracking commands change SQLite runtime
state. Read `tracking status` for the current tracking ETag before a mutation.
Its revision is independent of the catalog revision.

```sh
ha-closet tracking config validate --input tracking.json
ha-closet tracking config apply --input tracking.json \
  --if-match '"0"' --idempotency-key example-configuration
ha-closet tracking status
ha-closet tracking enroll start UNIT_UUID --port example-enroll \
  --if-match '"1"' --idempotency-key example-enrollment
ha-closet tracking history --unit UNIT_UUID --limit 50
ha-closet tracking coverage --limit 50
ha-closet tracking analytics --days 90 --pick-seed 8
ha-closet tracking export --output tracking-backup.json
ha-closet tracking replay --input config/tracking-replay.example.json
```

Use the actual returned ETag, UUID and capture version, not the illustrative
values above. `enroll confirm` requires session, unit, EPC and `--capture-version`;
`enroll capture` discards earlier candidates. `enroll resume` preserves a saved
queue but starts a fresh capture. `--replace` on enrollment retires the previous
tag without replacing the unit. Lookup, baseline, correct, unbind, retire and
undo are explicit subcommands. `tracking unbind --mistaken` also excludes that
association from analytics while preserving its audit; ordinary unbind retains
valid wear history. `tracking apply --input command.json` covers
other typed commands, including installation and quantity reconciliation.

Config validation and replay are offline, before credentials are resolved.
Replay always uses a new in-memory simulator, never the configured production
origin. Exports are private 0600 files, atomically created without overwriting
an existing path. Import requires a synced catalog with matching stable IDs and
the usual tracking revision/retry key. An import cannot remove or rewrite
existing movement history. History and coverage use `next_before` cursors; pass
them as `--before` to read older pages. Raw `observe` batches are administrator-
only and deduplicated by reader session/sequence. They are not ordinary catalog
writes. [Tracking setup, rules, recovery and hardware](rfid.md).

## Today

```sh
ha-closet today get
ha-closet today generate --occasion everyday \
  --weather-mode required \
  --idempotency-key outfit-attempt-1
ha-closet today get --format table
```

`today get` only reads saved output and the latest attempt. Generate returns
`202` when accepted, not when completed. Read again explicitly for status;
there is no hidden polling or automatic retry. Reusing the exact original key
and inputs reads that operation rather than paying for another generation.

Occasions are `all` (the default), `everyday`, `work`, `dinner` (Evening),
and `bedtime`. All requests up to three outfits per eligible group in one model
call. Work is very casual, approximately Everyday. Bedtime uses sleepwear
and does not require shoes. Weather is `required` unless explicitly set to
`ignored`. These settings are supplied to the server-owned request type; there
is no arbitrary prompt or model credential in the CLI.

Bedtime accepts `clothing.sleepwear.nightgown` or `clothing.sleepwear.set`, or a
`clothing.sleepwear.top` with a `clothing.sleepwear.bottom`. A
`clothing.sleepwear.robe` is an optional layer. Daytime suggestions exclude
sleepwear.

## Failures

Failures write one bounded JSON diagnostic to stderr and exit nonzero. Invalid
arguments exit 2; other failures exit 1. Diagnostics include HTTP status when
known and a stable `error_code`, not raw provider messages, URLs, credentials or
private input. Validation failures can also include item indexes and field
codes. Errors make no stdout output, except a broken output pipe can leave
partial output.

`--timeout` sets each Closet API request and native photo download deadline
in seconds, including the body (default 30, range 1–300). Home Assistant sync
has a one-hour network budget for large photo batches; native catalog sync
defaults to ten minutes.
`inventory sync --sync-timeout SECONDS` overrides that budget (1–7200 seconds).
Photo preparation runs locally before the network operation. Native WebSocket
commands have 15 seconds and one upload has 120 seconds.
Redirects are never followed. A timeout or lost reply does not prove that a
write was rejected. Only an exact, durable pending deletion can be resumed;
uploads and inventory writes are never automatically retried.
