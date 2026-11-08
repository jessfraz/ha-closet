# ha-closet

Validate a catalog locally, sync it to Closet, or read the app's inventory.
The CLI uses the same Rust types and validator as the service.

## Validate and sync

The source file is a JSON array. See [the fictional example](../config/catalog.example.json).

```sh
ha-closet inventory validate --input wardrobe.json
ha-closet inventory sync --input wardrobe.json --dry-run
```

Validation is offline. It does not resolve a server URL, read a credential,
connect to Home Assistant, or write anything. This is the command to run in CI.
Invalid input exits nonzero with safe field locations and error codes, without
printing item names or input values.

Sync sends the complete catalog, not a patch. A dry run returns the proposed
counts and current quoted catalog ETag. To apply, use that exact ETag and a
caller-owned idempotency key:

```sh
ha-closet inventory sync --input wardrobe.json \
  --if-match '"0"' --idempotency-key catalog-revision-1
```

Replace `"0"` with the ETag from the dry run. A sync creates or updates records
by stable ID, archives records absent from the source, and restores records
that reappear. It never permanently deletes records. An empty catalog needs
`--allow-empty`; an invalid catalog changes nothing.

The CLI never applies after a dry run, invents a key, or fetches a newer ETag
to make a failed write succeed. If a reply is lost, preserve the original body,
key, revision and authenticated identity before deciding whether to retry.

Use `--input -`, or omit `--input`, to read the JSON array from stdin. Input is
limited to 1 MiB. Files are local paths, never URLs.

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

The HA bridge supports catalog reads, sync and Today. Sync and generation
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

List filters are `--q`, `--brand`, `--category`, `--source` and `--confirmed-only`.
Category filters include their dot-delimited descendants. Source keys are
`shop:<id>`, `purchase:<value>`, or `unspecified`; copy an exact key from the JSON
`facets.sources` list. A shop takes precedence over a purchase source.
Each invocation fetches one page. Apply filters on the first request, then pass
the returned `next_page` value with `--page-token` and no filters. The cursor
carries the applied query; `--limit` and `--format` may change. The CLI never
drains pages or restarts an invalidated cursor.

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
  --ownership-scope all_inventory --weather-mode required \
  --idempotency-key outfit-attempt-1
ha-closet today get --format table
```

`today get` only reads saved output and the latest attempt. Generate returns
`202` when accepted, not when completed. Read again explicitly for status;
there is no hidden polling or automatic retry. Reusing the exact original key
and inputs reads that operation rather than paying for another generation.

Occasions are `everyday`, `work`, and `dinner`. Ownership scope is
`all_inventory` or `confirmed_only`. Weather is `required` unless explicitly
set to `ignored`. These settings are supplied to the server-owned request type;
there is no arbitrary prompt or model credential in the CLI.

## Failures

Failures write one bounded JSON diagnostic to stderr and exit nonzero. Invalid
arguments exit 2; other failures exit 1. Diagnostics include HTTP status when
known and a stable `error_code`, not raw provider messages, URLs, credentials or
private input. Validation failures can also include item indexes and field
codes. Errors make no stdout output, except a broken output pipe can leave
partial output.

`--timeout` sets the complete request deadline in seconds, including the body
(default 30, range 1–300). Redirects, network errors and HTTP failures are not
retried. A timeout or lost reply does not prove that a write was rejected.
