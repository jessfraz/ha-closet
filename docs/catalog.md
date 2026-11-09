# Catalog format

Keep these files together in private storage:

| File | Contents |
| --- | --- |
| `wardrobe.toml` | Styles, color variants and individual purchases |
| `brands.toml` | Canonical brand names, aliases and sizing profiles |
| `categories.toml` | Category labels and optional parent categories |
| `fittings.toml` | Fitting labels, seasons and years |
| `wardrobe-images/` | Local photos, named `<sha256>.png` |

The [fictional example](../config/wardrobe.example/wardrobe.toml) has two
purchases of the same tee. Their IDs and sizes stay separate. A shared variant
does not merge purchases or add their quantities together.

```sh
ha-closet inventory validate --input wardrobe.toml
ha-closet inventory sync --input wardrobe.toml --dry-run
ha-closet inventory sync --input wardrobe.toml
```

## References and names

Styles reference brand and category IDs. Variants reference styles. Purchases
reference variants and either a fitting ID or `self_bought`. Unknown references,
duplicate IDs and unknown fields fail validation.

Every purchase requires size and positive quantity. Its effective variant
requires color, materials and at least one photo. Nothing defaults to quantity
1. Use `one_size` only when that is the reported size, not as a missing-size
placeholder. An unknown purchase price is omitted, never replaced with zero or a
retail reference. Manufacturer SKUs and extra provenance may be omitted.

For an individual purchase reserved for safety work, set:

```toml
usage = { kind = "safety_work_only", requirement = "steel_toe" }
```

This user-defined restriction stays visible in the inventory. Normal Today
requests, including casual Work, exclude the item. An explicit steel-toe
footwear requirement admits matching, available safety footwear and excludes
ordinary shoes. It records intended use and the steel-toe requirement, not a
certification or a suitability guarantee.
Omitting `usage` preserves everyday use and the existing JSON representation.

The API projects stable authoring IDs as `identity = { style_id, variant_id }`.
Inventory cards can group purchases sharing a variant while preserving each
purchase ID, quantity, size, source and physical-unit identity. Legacy JSON
items without these IDs stay independent; names and photos never define groups.
Shared variant IDs require matching brand, category, color and material.
Purchase-specific names, photos, sizes, sources, prices and use restrictions
can still differ.

Use one brand entry for every spelling of that brand. Aliases support imports
and search; author references with canonical IDs. Unicode-normalized name and
alias collisions are errors, not last-one-wins replacements. Display names stay
human-readable while enum tokens use one spelling, such as `off_white` and `xs`.

## Colors, sizes and materials

Generic colors use the closed color-family enum. Keep an evidenced manufacturer
shade separately. Adding a shade never silently changes the displayed color.
Slash-joined guesses are not a substitute for a known color.

Sizes have an explicit sizing profile and value kind. Quote numeric labels:
`"00"` is different from `"0"`. A reported UK/US pair keeps both labels; the
compiler does not calculate conversions or guess who wears an item. Footwear
labels also support exact thirds, such as `"41 1/3"` and `"41 2/3"`; these
remain fractions, and profile bounds compare them without rounding. A missing
size fails validation; do not invent a size to satisfy the check.

Materials are structured per component, such as shell, lining or sole. A listed
fiber does not imply 100% purity. Percentage compositions use integer basis
points: 7700 means 77%. Complete compositions total 10000 per component. Denim,
satin and terry describe construction, not a guessed fiber. Known qualifiers,
such as recycled or organic, stay attached to their constituent.

Labels are consistent: `Cashmere` does not become a second material because
another item explicitly reports 100% cashmere. The exact composition remains
in the source.

Leather labels keep their material noun: `Pebbled leather`, not just `Pebbled`.
Names such as `Suede` and `Calfskin` already identify the material and stay as-is.

## Photos

Keep supplier URLs as `remote` images. An edited local photo uses `asset` and
its SHA, with the PNG in `wardrobe-images/`. No Home Assistant image IDs or host
URLs belong in the source. Each image retains its provenance and optional crop.

Variants share galleries by default. A purchase can replace its gallery;
an explicit empty gallery means no photos, not inheritance.

Validation checks every registry and local asset offline. Sync resolves real
native URLs in memory, applies the catalog, verifies it and removes obsolete
Closet-managed photos. It never rewrites the source or uses made-up image IDs.
Changing an input while sync runs aborts before catalog commit or cleanup.

Existing JSON catalogs and their image manifests remain supported. The HTTP
API still uses its existing flat JSON shape.
