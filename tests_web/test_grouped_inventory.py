"""Stable variant cards keep complete purchases across pages and filters."""

import asyncio
import json
import re
import sqlite3
from pathlib import Path

from homeassistant.util.json import JsonObjectType
from playwright.async_api import async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import command, snapshot, units
from tests_web.test_app import login
from tests_web.test_inventory_ui import product_photo


async def test_grouped_copies_are_complete_before_later_pages_and_keep_details(
    rust_backend: RustBackend,
) -> None:
    first = catalog_item("example-000", name="Example grouped top")
    second = catalog_item("example-999", name="Example grouped top")
    for record, size in ((first, "M"), (second, "L")):
        record["identity"] = {
            "style_id": "example-style",
            "variant_id": "example-variant",
        }
        record["size"] = size
        record["images"] = [
            {
                "url": "https://images.example.invalid/garment.svg",
                "kind": "retailer_product_reference",
                "confidence": "high",
            }
        ]
    second["shop"] = {
        "id": "example-atelier",
        "label": "Example atelier",
        "season": None,
        "years": [],
    }
    # Identical display names must never collapse unrelated legacy purchases.
    records = [
        first,
        *[
            catalog_item(f"example-{index:03}", name="Example grouped top")
            for index in range(1, 50)
        ],
        second,
    ]
    await rust_backend.sync(records)
    physical = units(await snapshot(rust_backend))
    second_unit = next(unit for unit in physical if unit["catalog_id"] == second["id"])
    for record, status in ((first, "clean_ready"), (second, "needs_laundry")):
        unit = next(unit for unit in physical if unit["catalog_id"] == record["id"])
        await command(
            rust_backend,
            {
                "type": "correct",
                "unit_id": unit["id"],
                "status": status,
                "note": "Fictional state for the copy-details contract.",
            },
        )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": 390, "height": 844}, color_scheme="dark"
            )
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend, manual_paging=True)
            await expect(page.locator(".item-card")).to_have_count(50)
            group = page.locator('[data-variant-id="example-variant"]')
            await expect(group.locator(".owned-count")).to_have_text("2 owned")
            await expect(group.locator(".item-copy")).to_have_count(2)
            await expect(group.get_by_text("Size M", exact=True)).to_be_visible()
            await expect(group.get_by_text("Size L", exact=True)).to_be_visible()
            await expect(group.locator(".source-badge")).to_have_text(
                ["Self-bought", "Example atelier"]
            )
            await expect(
                group.locator('[data-copy-id="example-000"] .tracking-pill')
            ).to_have_text("Clean / ready")
            await expect(
                group.locator('[data-copy-id="example-999"] .tracking-pill')
            ).to_have_text("Needs laundry")
            second_copy = group.get_by_role(
                "button", name="View Example grouped top, purchase 2", exact=True
            )
            await expect(second_copy).to_have_accessible_description(
                "Size L · Quantity 1 · Example atelier"
            )
            await second_copy.focus()
            await page.keyboard.press("Enter")
            dialog = page.get_by_role("dialog")
            await expect(dialog).to_be_visible()
            await expect(
                dialog.locator("dd").filter(has_text=re.compile("^L$"))
            ).to_be_visible()
            await expect(dialog.locator(".tracking-unit")).to_have_count(1)
            await expect(
                dialog.locator(".tracking-unit-heading .tracking-pill")
            ).to_have_text("Needs laundry")
            async with page.expect_request(
                lambda request: (
                    f"/v1/tracking/units/{second_unit['id']}/history?" in request.url
                )
            ):
                await dialog.get_by_text("Movement and corrections", exact=True).click()
            await expect(dialog.locator(".tracking-history")).to_be_visible()
            await expect(page).to_have_url(re.compile("closet_item=example-999"))
            await page.get_by_role("button", name="Close item details").click()
            await expect(second_copy).to_be_focused()
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(
                page.get_by_role("button", name="Load more", exact=True)
            ).to_be_hidden()
            await expect(page.locator(".item-card")).to_have_count(50)
            await expect(group.locator(".item-open")).to_be_focused()
            await expect(group.locator(".owned-count")).to_have_text("2 owned")
            await expect(group.locator(".item-copy")).to_have_count(2)
            await group.get_by_role(
                "button", name="Show all items from Example atelier", exact=True
            ).click()
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(page.locator("#inventory-count")).to_have_text(
                "1 item · 51 in inventory"
            )
            await expect(group.locator(".owned-count")).to_have_text("2 owned")
            await expect(group.locator(".item-copy")).to_have_count(2)
            await expect(page).to_have_url(
                re.compile("closet_source=shop%3Aexample-atelier")
            )
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            assert not await group.locator("button button, button a").count()
        finally:
            await browser.close()


async def test_unknown_group_quantity_and_distinct_variants_remain_explicit(
    rust_backend: RustBackend,
    tmp_path: Path,
) -> None:
    records = [
        catalog_item(f"example-copy-{index}", name="Example same-name shirt")
        for index in range(4)
    ]
    for index, record in enumerate(records[:3]):
        record["identity"] = {
            "style_id": "example-style",
            "variant_id": "example-blue" if index < 2 else "example-grey",
        }
    records[1]["quantity"] = 2
    bulk = catalog_item("example-bulk", name="Example multipack")
    bulk["identity"] = {"style_id": "example-pack", "variant_id": "example-pack-blue"}
    bulk["quantity"] = 3
    records.append(bulk)
    await rust_backend.sync(records)

    def historical_quantity() -> None:
        # New imports require a quantity. Seed only this disposable DB into the
        # supported historical nullable state, including its physical projection.
        with sqlite3.connect(tmp_path / "catalog.sqlite3") as database:
            database.execute("PRAGMA foreign_keys=ON")
            database.execute("BEGIN IMMEDIATE")
            saved = database.execute(
                "SELECT body FROM items WHERE id=?", (records[0]["id"],)
            ).fetchone()
            assert saved is not None and isinstance(saved[0], str)
            body: JsonObjectType = json.loads(saved[0])
            body["quantity"] = None
            database.execute(
                "DELETE FROM tracking_history WHERE unit_id IN "
                "(SELECT id FROM tracking_units WHERE catalog_id=?)",
                (records[0]["id"],),
            )
            database.execute(
                "DELETE FROM tracking_units WHERE catalog_id=?", (records[0]["id"],)
            )
            database.execute(
                "UPDATE items SET body=? WHERE id=?",
                (json.dumps(body), records[0]["id"]),
            )
            database.execute(
                "UPDATE tracking_inventory SET catalog_quantity=NULL,"
                "declared_quantity=NULL,next_ordinal=1,needs_reconciliation=1 "
                "WHERE catalog_id=?",
                (records[0]["id"],),
            )
            database.execute(
                "UPDATE tracking_state SET revision=revision+1 WHERE singleton=1"
            )

    await asyncio.to_thread(historical_quantity)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(4)
            blue = page.locator('[data-variant-id="example-blue"]')
            await expect(blue.locator(".owned-count")).to_have_text(
                "Quantity not recorded"
            )
            await expect(blue.locator(".item-copy")).to_have_count(2)
            await expect(blue.locator(".copy-open")).to_contain_text(
                ["View copy", "View copy"]
            )
            assert "Size not recorded" not in await blue.inner_text()
            await expect(
                blue.get_by_text("· quantity unknown", exact=True)
            ).to_be_visible()
            await expect(blue.get_by_text("· ×2", exact=True)).to_be_visible()
            grey = page.locator('[data-variant-id="example-grey"]')
            await expect(grey.locator(".owned-count, .item-copies")).to_have_count(0)
            multipack = page.locator('[data-variant-id="example-pack-blue"]')
            await expect(multipack.locator(".owned-count")).to_have_text("3 owned")
            await expect(multipack.locator(".item-copy")).to_have_count(1)
            await expect(
                page.locator('.item-card[data-catalog-id="example-copy-3"]')
            ).to_be_visible()
        finally:
            await browser.close()
