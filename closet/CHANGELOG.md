# Changelog

## 0.4.11

Keep version Repairs actionable when the first data refresh fails, using a fresh
entry-scoped version check without requiring a loaded coordinator.
Requires Home Assistant 2026.11.

## 0.4.10

- Show specific version repair failures and allow retrying the version check.

## 0.4.9

- Match the Home Assistant App and Marketplace icons to the sidebar.

## 0.4.8

- Repair app and companion version mismatches from Home Assistant Repairs.

## 0.4.6

- Recover private files and pairing credentials after native restores, without
  following external links or changing nested mounts.

## 0.4.5

- Share the app shell, controls, and themes with HA App Kit.
- Keep home navigation in the current tab and improve empty photo contrast.

## 0.3.3

- Renew the sync lease while verifying old photos, so large cleanup batches
  finish without weakening revision checks or deletion safeguards.

## 0.3.2

- Honor the request timeout when verifying native photos, including large old
  originals before cleanup. Sync still checks exact bytes before deleting them.

## 0.3.1

- Give large Home Assistant photo syncs a one-hour network budget and a bounded
  override. Image preparation, upload, inventory verification and cleanup remain
  one command. Per-request deadlines and safe recovery still apply.

## 0.3.0

- Add an Analytics page with wear trends, most-worn pieces and overlooked picks.
  Untracked time and reader outages do not count as neglect.
- Optimize local image uploads during validation and sync, with bounded sizes,
  color-aware resizing and unchanged source files.
- Protect newly referenced photos when resuming an interrupted cleanup, and
  reconcile earlier upload claims before changing delivery policy.
- Exclude mistaken tag associations from analytics without deleting the audit.
- Retain garment history across tracking imports and record restored states.
- Keep verified coverage intervals so later wear statistics can exclude tracking
  gaps, without storing every radio read.
- Use generic installation wording and the `queued` installation state. Existing
  backups remain readable; API clients should update for the canonical value.

## 0.2.0

- Pair UHF tags with physical copies of the existing inventory, using photos
  rather than retyping garment details.
- Infer one garment status from confirmed area scans, with conservative outage
  recovery and a separate Ready to wear view. No model makes these decisions.
- Add reader diagnostics, installation exceptions, tag replacement, audited
  corrections, private exports and an isolated replay simulator.
- Project counts, health and optional per-piece status sensors into HA.

## 0.1.13

- Load native image previews in grids and keep full-resolution item galleries.

## 0.1.12

- Keep material nouns in labels such as Pebbled leather and Bovine leather.

## 0.1.11

- Author catalogs in TOML with checked brand, category and fitting references,
  shared styles and color variants, and separate purchase records.
- Standardize colors, sizes and materials with validated Rust types.
- Require an explicit positive quantity on every new import.
- Validate and sync TOML and local photos with the same CLI commands. Existing
  JSON catalogs, inventory IDs and saved outfits remain compatible.

## 0.1.10

- Sync local photos and inventory with one Rust CLI command, then remove obsolete
  Closet-managed Home Assistant photos after verified readback.
- Keep uncertain uploads and pending cleanup safe across retries and restarts.
- Generate Everyday, casual Work, Evening and Bedtime groups with Today’s All
  option in one request.

## 0.1.9

- Use a white photo canvas in every view, including the dark theme.
- Keep long source labels compact in Today.
- Filter from badges without focusing the native filter dropdown.
- Give Today the local calendar context, and add a sleepwear-only bedtime option.

## 0.1.8

- Keep uncropped photos fully visible in inventory cards and galleries.

## 0.1.7

- Remove purchase condition from the catalog and app. Existing databases upgrade
  automatically without losing inventory, saved outfits or retry history.

## 0.1.6

- Display faithful image crops without changing or hosting the original photo.

## 0.1.5

- Keep filters and item details in the URL, including Home Assistant ingress.
- Browse multiple item photos with thumbnails and previous/next controls.
- Remove redundant inventory and item-detail instructions.

## 0.1.4

- Put the brand badge above the item name; keep the source at the bottom.
- Remove the redundant reference-photo label.

## 0.1.3

- Remove ownership confirmation throughout the app. Inventory membership means
  owned.
- Hide missing sizes, compact the source badges, and load more items on scroll.
- Remove the Unspecified source category and filter.
- Require a shop or purchase source when validating and syncing a catalog.

## 0.1.2

- Match phone header spacing and stacked footers across the apps.

## 0.1.1

- Click brand and source badges to browse matching inventory.
- Typed shop seasons and source filters in the API and CLI.
- Role-constrained choices for the optional outfit lottery.
- Cursor-only paging that fits Home Assistant request limits.

## 0.1.0

- JSON-owned inventory, offline validation and revision-checked snapshot sync.
- Inventory browser, native CLI and Home Assistant integration.
- Optional on-demand outfit lottery through Home Assistant AI Tasks.
