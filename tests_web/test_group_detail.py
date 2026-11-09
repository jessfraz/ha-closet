"""A shared variant opens all acquisitions; copy links keep exact identities."""

import asyncio
import re

import pytest
from playwright.async_api import Error, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import command, snapshot, units
from tests_web.test_app import login
from tests_web.test_inventory_ui import product_photo


async def prepare_group(backend: RustBackend) -> dict[str, str]:
    """Different sources and sizes, including one purchase of three pieces."""
    self_bought = catalog_item("example-000", name="Example grouped tee")
    fitting = catalog_item("example-998", name="Example grouped tee")
    bulk = catalog_item("example-999", name="Example grouped tee")
    for record, size, quantity, price in (
        (self_bought, "S", 1, 48),
        (fitting, "S", 1, 64),
        (bulk, "M", 3, 45),
    ):
        record.update(
            identity={"style_id": "example-style", "variant_id": "example-variant"},
            size=size,
            quantity=quantity,
            price={
                "amount": price,
                "currency": "USD",
                "basis": "receipt_line_price",
                "quantity_basis": "unit",
            },
            images=[
                {
                    "url": "https://images.example.invalid/garment.svg",
                    "kind": "retailer_product_reference",
                    "confidence": "high",
                }
            ],
            notes=[f"Fictional acquisition {record['id']}."],
        )
    fitting["shop"] = {
        "id": "example-atelier",
        "label": "Example atelier",
        "season": "Spring",
        "years": [2026],
    }
    await backend.sync(
        [
            self_bought,
            *[
                catalog_item(f"example-{index:03}", name="Example grouped tee")
                for index in range(1, 50)
            ],
            fitting,
            bulk,
        ]
    )
    physical = units(await snapshot(backend))
    result: dict[str, str] = {}
    for record, status in ((self_bought, "clean_ready"), (fitting, "needs_laundry")):
        unit = next(unit for unit in physical if unit["catalog_id"] == record["id"])
        unit_id = unit["id"]
        assert isinstance(unit_id, str)
        result[str(record["id"])] = unit_id
        await command(
            backend,
            {
                "type": "correct",
                "unit_id": unit_id,
                "status": status,
                "note": "Fictional acquisition-specific status.",
            },
        )
    return result


async def test_shared_image_opens_all_copies_and_restores_group_bookmark(
    rust_backend: RustBackend,
) -> None:
    await prepare_group(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend, manual_paging=True)
            group = page.locator('[data-variant-id="example-variant"]')
            await expect(
                group.get_by_text("All purchases shown", exact=True)
            ).to_have_count(0)
            await group.locator(".photo").scroll_into_view_if_needed()
            photo = await group.locator(".photo").bounding_box()
            assert photo
            await page.mouse.click(
                photo["x"] + photo["width"] / 2, photo["y"] + photo["height"] / 2
            )
            await expect(page).to_have_url(re.compile("closet_variant=example-variant"))
            assert "closet_item=" not in page.url
            dialog = page.get_by_role("dialog", name="Example grouped tee")
            await expect(
                dialog.get_by_text("5 pieces · 3 purchases", exact=True)
            ).to_be_visible()
            await expect(
                dialog.get_by_text("Example atelier", exact=True)
            ).to_be_visible()
            await expect(
                dialog.get_by_text("Self-bought", exact=True).first
            ).to_be_visible()
            bookmark = page.url
            await page.reload()
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await expect(page).to_have_url(bookmark)
            await expect(
                dialog.get_by_text("5 pieces · 3 purchases", exact=True)
            ).to_be_visible()
            await page.get_by_role(
                "button", name="Close item details", exact=True
            ).click()
            await expect(dialog).to_be_hidden()
            assert "closet_variant=" not in page.url
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


async def test_same_size_is_one_row_with_distinct_acquisitions_under_source_filter(
    rust_backend: RustBackend,
) -> None:
    await prepare_group(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 320, "height": 844})
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend, manual_paging=True)
            group = page.locator('[data-variant-id="example-variant"]')
            await expect(group.get_by_text("Size S", exact=True)).to_have_count(1)
            await expect(group.locator(".owned-count")).to_have_text(
                "5 pieces · 3 purchases"
            )
            await group.get_by_role(
                "button", name="Show all items from Example atelier", exact=True
            ).click()
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(group.get_by_text("Size S", exact=True)).to_have_count(1)
            await expect(group.locator(".owned-count")).to_have_text(
                "5 pieces · 3 purchases"
            )
            await expect(
                group.get_by_text("All purchases shown", exact=True)
            ).to_be_visible()
            await expect(group.locator(".source-match")).to_have_text("Example atelier")
            await expect(
                group.get_by_role(
                    "button", name="Show all items from Self-bought", exact=True
                ).first
            ).to_be_visible()
            await group.get_by_role(
                "button", name="View all copies of Example grouped tee", exact=True
            ).click()
            await expect(page).to_have_url(re.compile("closet_variant=example-variant"))
            await expect(
                page.get_by_role("dialog").get_by_text(
                    "5 pieces · 3 purchases", exact=True
                )
            ).to_be_visible()
            await expect(
                page.get_by_role("dialog").locator(".item-copy")
            ).to_have_count(3)
            await page.get_by_role("button", name="Close item details").click()
            assert "closet_source=shop%3Aexample-atelier" in page.url
            assert "closet_variant=" not in page.url
        finally:
            await browser.close()


@pytest.mark.parametrize("width", [320, 1440])
async def test_exact_purchase_keeps_source_price_units_history_and_back_path(
    rust_backend: RustBackend,
    width: int,
) -> None:
    unit_ids = await prepare_group(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend, manual_paging=True)
            group = page.locator('[data-variant-id="example-variant"]')
            await group.get_by_role(
                "button", name="Show all items by Example label"
            ).focus()
            await page.keyboard.press("Tab")
            main = group.get_by_role(
                "button", name="View all copies of Example grouped tee", exact=True
            )
            await expect(main).to_be_focused()
            await page.keyboard.press("Enter")
            dialog = page.get_by_role("dialog", name="Example grouped tee")
            await expect(dialog.locator(".item-copy")).to_have_count(3)
            await expect(dialog.locator(".tracking-detail")).to_have_count(0)
            await expect(
                dialog.locator('[data-copy-id="example-998"] .purchase-facts')
            ).to_contain_text("$64.00")
            await expect(
                dialog.locator('[data-copy-id="example-000"] .purchase-facts')
            ).to_contain_text("$48.00")
            copy_button = dialog.locator('[data-copy-id="example-998"] .copy-open')
            await copy_button.focus()
            await page.keyboard.press("Enter")
            await expect(page).to_have_url(re.compile("closet_item=example-998"))
            assert "closet_variant=" not in page.url
            await expect(dialog.locator(".tracking-unit")).to_have_count(1)
            await expect(dialog.get_by_text("$64.00", exact=True)).to_be_visible()
            await expect(
                dialog.get_by_text("Example atelier", exact=True)
            ).to_be_visible()
            await expect(dialog.get_by_text("Spring", exact=True)).to_be_visible()
            await expect(
                dialog.get_by_text("Fictional acquisition example-998.", exact=True)
            ).to_be_visible()
            await expect(
                dialog.get_by_text("Fictional acquisition example-000.", exact=True)
            ).to_have_count(0)
            await expect(
                dialog.locator(".tracking-unit-heading .tracking-pill")
            ).to_have_text("Needs laundry")
            async with page.expect_request(
                lambda request: (
                    f"/v1/tracking/units/{unit_ids['example-998']}/history?"
                    in request.url
                )
            ):
                await dialog.get_by_text("Movement and corrections", exact=True).click()
            await expect(dialog.locator(".tracking-history")).to_be_visible()
            bookmark = page.url
            await page.reload()
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await expect(page).to_have_url(bookmark)
            await expect(dialog.get_by_text("$64.00", exact=True)).to_be_visible()
            await dialog.get_by_role("button", name="‹ All copies", exact=True).click()
            await expect(page).to_have_url(re.compile("closet_variant=example-variant"))
            await expect(dialog.locator(".item-copy")).to_have_count(3)
            await page.go_back()
            await expect(dialog).to_be_hidden()
            assert "closet_item=" not in page.url and "closet_variant=" not in page.url
            await main.click()
            await dialog.locator('[data-copy-id="example-999"] .copy-open').click()
            await expect(page).to_have_url(re.compile("closet_item=example-999"))
            await expect(dialog.locator(".tracking-unit")).to_have_count(3)
            await expect(dialog.get_by_text("$45.00", exact=True)).to_be_visible()
            await page.go_back()
            await expect(page).to_have_url(re.compile("closet_variant=example-variant"))
            await expect(dialog.locator(".item-copy")).to_have_count(3)
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            assert not await page.locator("button button, button a").count()
        finally:
            await browser.close()


async def test_missing_variant_bookmark_is_recoverable(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.goto(
                rust_backend.origin
                + "/?closet_variant=example-missing&closet_brand=Example+label"
            )
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            dialog = page.get_by_role("dialog", name="Item unavailable", exact=True)
            await expect(dialog).to_be_visible()
            await dialog.get_by_role("button", name="Close item details").click()
            await expect(dialog).to_be_hidden()
            await expect(page.locator(".item-card")).to_have_count(1)
            assert "closet_brand=Example+label" in page.url
            assert "closet_variant=" not in page.url
        finally:
            await browser.close()


async def test_back_and_exact_purchase_ignore_late_variant_response(
    rust_backend: RustBackend,
) -> None:
    await prepare_group(rust_backend)
    started = asyncio.Event()
    release = asyncio.Event()

    async def delayed(route: Route) -> None:
        response = await route.fetch()
        started.set()
        await release.wait()
        try:
            await route.fulfill(response=response)
        except Error:
            # Back cancels this request; only its real native response is held.
            pass

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://images.example.invalid/**", product_photo)
            await page.route("**/v1/variants/example-variant/items", delayed)
            await login(page, rust_backend, manual_paging=True)
            group = page.locator('[data-variant-id="example-variant"]')
            await group.locator(".group-open").click()
            async with asyncio.timeout(5):
                await started.wait()
            await page.go_back()
            await expect(page.get_by_role("dialog")).to_be_hidden()
            await group.locator('[data-copy-id="example-998"] .copy-open').click()
            dialog = page.get_by_role("dialog", name="Example grouped tee")
            await expect(dialog.get_by_text("$64.00", exact=True)).to_be_visible()
            release.set()
            await page.wait_for_timeout(150)
            await expect(page).to_have_url(re.compile("closet_item=example-998"))
            await expect(dialog.locator(".tracking-unit")).to_have_count(1)
            await expect(dialog.locator(".group-detail")).to_have_count(0)
        finally:
            release.set()
            await browser.close()
