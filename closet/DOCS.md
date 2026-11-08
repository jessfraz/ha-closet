# Using Closet

## Setup

Install the app and the integration from this repository using Marketplace
(or HACS on older Home Assistant versions). Start the app,
restart Home Assistant after installing the integration, then confirm the
discovered connection in Settings → Devices & services. App/integration versions
must match. The app's sidebar is restricted to administrators.

The optional native API port is disabled by default. Home Assistant-backed
CLI clients use the integration's authenticated bridge and need no extra port.
Standalone containers use bearer authentication. Keep credentials outside the
checkout and use the CLI's token-file or environment support.

## Options

| Option | Purpose |
| --- | --- |
| timezone | IANA timezone for Today. Use Home Assistant's configured timezone. |
| weather_entity | Weather entity to use for the optional outfit lottery. |
| ai_task_entity | AI Task entity; empty uses Home Assistant's preferred AI Task. |

Empty model/weather configuration does not block inventory or sync. A request
that requires an unavailable forecast fails clearly; Ignore forecast is an
explicit choice, not a silent fallback.

## Inventory

Edit item metadata and image references in a private TOML catalog. There is no
separate Add/Edit workflow in the app. Validate the source, preview the diff
and sync the reviewed snapshot. Existing JSON catalogs also work.
Every item in the catalog is owned.

IDs are stable purchase identities, not descriptions or SKUs. Omit unknown
details in TOML. Sync updates the projection in one transaction, archives absent IDs,
and reactivates IDs restored to the file. An empty snapshot requires an explicit
allow-empty option. Invalid input, a stale revision or a conflicting retry key
does not replace the catalog.

Older JSON files must drop `current_ownership_confirmed` and
`condition_at_purchase`. Existing databases upgrade automatically and keep
their inventory, saved outfits and retry records. A historical sync key whose
original snapshot is no longer recoverable stays reserved and rejects
mismatched retries. Preview the current file with a new key for a new sync.

Photos can be supplier HTTPS references or private SHA-addressed PNGs synced
with the catalog. Broken photos show a placeholder. Use images that match the
item's style and color. See the [CLI reference](../docs/cli.md) for photo sync.

An image can carry an optional display-only crop:
`"crop": {"x": 2500, "y": 1250, "width": 5000, "height": 7500}`.
Coordinates are integers in 1/10,000 of the original width and height, measured
from the top-left. Width and height must be positive; `x + width` and
`y + height` cannot exceed 10000. The example selects the middle half of the
width and three quarters of the height. The same viewport appears on cards
and in the gallery. The original URL, source link and image pixels stay intact.
Omit `crop` to show the full image.

## Garment tracking

Open Tracking to configure readers, enroll existing garments from their photos,
or prepare an installation queue. Tracking starts disabled. Passive UHF tags
need a UHF reader; a phone or tablet is only the pairing interface.

The app infers one status from confirmed closet, rewear, hamper or dry-cleaning
holder evidence. Readiness does not change which items remain in the inventory.
Reader health is separate from garment state. No LLM makes tracking decisions.
See [RFID setup and recovery](../docs/rfid.md) before commissioning hardware.

Reader credentials can be supplied in the owner-only
`/data/reader-secrets.json` reference map. A private reader CA belongs in
`/data/reader-ca.crt`; normal certificate verification stays enabled.
Neither file belongs in a public repository. Restart after changing either file.

## Today

Today calls the configured HA AI Task only on request. GETs and page visits do
not call a model. The selected provider receives a bounded text-only catalog
shortlist and the selected occasion/weather context. It does not receive photo
files, receipts, source notes, shop metadata or prices.

Model output is advisory. The app validates item IDs and outfit roles but
cannot prove that an outfit is good.
Failed refreshes preserve the previous result with its own inputs and freshness.
A timeout does not prove the upstream model stopped; retry the same operation
key to reconcile it before deliberately starting a new attempt.

## Backups

The app uses a cold backup. Native Home Assistant backups include its data:
SQLite catalog projection, sync/generation retry state, tracking unit identities,
active/retired tags, configuration, movement history and pairing credentials.
Private reader credentials and trust files in `/data` are backed up too.
Keep the catalog, registries and local source photos in your private storage
as well. Catalog files alone do not restore pairing or runtime state.

## Troubleshooting

Start with integration diagnostics and the CLI's system/status output. Reports
contain versions, counts and health, not item details or tokens. Keep successful
CLI inventory output private. If the model is unavailable, inventory remains
usable. For security problems, use private vulnerability reporting rather than
posting catalogs or credentials publicly.
