# Changelog

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
