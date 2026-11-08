# Closet

A wardrobe inventory for Home Assistant, backed by a JSON file.

The file is the source of truth. Closet validates it, syncs changes into a
local database, and gives it a searchable photo grid. No second catalog to
keep up to date in the app.

Select a brand or source badge to see its inventory. The filters can also be
combined to narrow things down.
You can bookmark a filtered view or an item. Items with multiple photos have
a thumbnail gallery.

![Closet inventory](docs/screenshots/inventory.png)

## Install

Add this repository to the **Home Assistant App Store** and to
**Settings → Marketplace → Add from link** as an integration. On older Home
Assistant versions, use HACS:

```text
https://github.com/jessfraz/ha-closet
```

Install and start the Closet app, install the integration, and restart Home
Assistant if prompted. Confirm its discovered connection in **Settings →
Devices & services**. Enable **Show in sidebar** to open the inventory.

See the [app documentation](closet/DOCS.md) for configuration and backups.

## Keep a catalog

Start with the [fictional example](config/catalog.example.json). Keep the real
file in your own private storage, not in a fork of this public repository.

```sh
ha-closet inventory validate --input wardrobe.json
ha-closet inventory sync --input wardrobe.json --dry-run
```

Each item needs a shop or purchase source.

Validation is offline. It needs no running service, credentials, or model.
Sync checks the same format and applies the whole snapshot atomically. Missing
IDs are archived, not deleted. Invalid files leave the current inventory alone.

Apply the file with one command:

```sh
ha-closet inventory sync --input wardrobe.json
```

With a Home Assistant connection, a sibling `wardrobe-images.json` manifest
and `wardrobe-images/` PNG directory are validated and synced too. Existing
exact copies are reused. After inventory readback, obsolete Closet-managed
photos are removed; other Home Assistant images are left alone. The source
files are never rewritten. See the [CLI reference](docs/cli.md) for the image
manifest and explicit revision/retry controls.

Install the CLI from the [release](https://github.com/jessfraz/ha-closet/releases)
or with Nix. It has JSON output, optional table output, and native or
Home Assistant connections. [CLI reference](docs/cli.md).

## Today

The inventory, validation, and sync are deterministic. Today is an optional
outfit lottery. It can ask Home Assistant's configured AI Task for a few
combinations from the catalog; whether anyone wears them is another question.
Choose All for Everyday, casual Work, Evening, and Bedtime groups.

It runs only when requested. The model gets item descriptions and, optionally,
the weather, not your photos or purchase evidence. The app checks that the
suggested pieces actually exist. No model is needed for the inventory.

## Development

```sh
nix develop
just check
```

Rust owns the API, SQLite, and CLI. Python connects it to Home Assistant.
`just security` checks dependencies; `just container` exercises restart and
backup/restore with Docker. [Contributing](CONTRIBUTING.md).

The example catalog is fictional. Screenshots use product-reference photos and
made-up shop labels, without sizes, prices, or purchase history.
[MIT](LICENSE).
