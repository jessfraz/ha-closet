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

Validation checks mappings, hashes, PNG headers and dimensions, regular files,
512 assets at most, 10 MiB minus 1 KiB per file and 512 MiB total. Unlisted
files and symlinks fail. Keep local photos and their manifest in private storage.
URL-only vendor references need no local file or manifest.

Sync reuses a native image only after exact-byte SHA verification. A missing
copy gets one multipart upload. A lost reply is reconciled through native
image/list and exact-byte readback, never a blind second POST. An unresolved
outcome returns `image_upload_pending`; rerunning can recover an existing
copy. If it remains absent, inspect the original operation before explicitly
resolving its durable claim. No uploads or cleanup happen during validation.

After successful inventory and full readback, cleanup removes only exact
`closet-<sha256>.png` objects whose bytes match that name. Active records and
currently displayed Today references, including archived records and native
thumbnail aliases, protect shared images. Unrelated HA images are untouched.
A durable server fence blocks all inventory writers while admitted deletions
are pending, even after a crash or lease expiration. `cleanup_pending` means
rerun the same command to reconcile/resume those exact IDs. Failures before
cleanup never authorize a new deletion batch.

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

List filters are `--q`, `--brand`, `--category` and `--source`.
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

`--timeout` sets each Closet API request deadline in seconds, including the
body (default 30, range 1–300). Complete sync has a ten-minute budget; native
reads/WebSocket commands have 15 seconds and one upload has 120 seconds.
Redirects are never followed. A timeout or lost reply does not prove that a
write was rejected. Only an exact, durable pending deletion can be resumed;
uploads and inventory writes are never automatically retried.
