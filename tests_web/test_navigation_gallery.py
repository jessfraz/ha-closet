"""Bookmark real catalog views and browse every product photo."""

import asyncio
from urllib.parse import parse_qs, urlencode, urlsplit

import pytest
from playwright.async_api import Error, Page, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_inventory_ui import product_photo
from tests_web.test_sources import source_records


async def connect_at(page: Page, backend: RustBackend, query: str = "") -> None:
    await page.add_init_script("delete globalThis.IntersectionObserver;")
    await page.goto(backend.origin + "/" + query)
    await reconnect(page, backend)


async def reconnect(page: Page, backend: RustBackend) -> None:
    await page.get_by_label("API token", exact=True).fill(backend.owner_token)
    await page.get_by_role("button", name="Connect", exact=True).click()
    await expect(page.locator("#inventory-heading")).to_be_visible()


async def test_applied_filters_bookmark_reload_and_history(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await connect_at(page, rust_backend, "?panel=keep#catalog")
            await page.get_by_label("Brand", exact=True).select_option("Label A")
            await page.get_by_label("Source", exact=True).select_option(
                "shop:demo-atelier"
            )
            await page.get_by_label("Category", exact=True).select_option(
                "clothing.top"
            )
            applied_url = page.url
            await page.get_by_label("Search", exact=True).fill("piece 00")
            assert page.url == applied_url
            await page.get_by_role("button", name="Search", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(5)
            query = parse_qs(urlsplit(page.url).query)
            assert query == {
                "panel": ["keep"],
                "closet_brand": ["Label A"],
                "closet_category": ["clothing.top"],
                "closet_source": ["shop:demo-atelier"],
                "closet_q": ["piece 00"],
            }
            assert urlsplit(page.url).fragment == "catalog"
            assert rust_backend.owner_token not in page.url
            await page.reload()
            await reconnect(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(5)
            await expect(page.get_by_label("Search", exact=True)).to_have_value(
                "piece 00"
            )
            await expect(page.get_by_label("Brand", exact=True)).to_have_value(
                "Label A"
            )
            await page.go_back()
            await expect(page.locator(".item-card")).to_have_count(32)
            await expect(page.get_by_label("Search", exact=True)).to_have_value("")
            await page.go_forward()
            await expect(page.locator(".item-card")).to_have_count(5)
            assert not await page.evaluate(
                "localStorage.length + sessionStorage.length"
            )
        finally:
            await browser.close()


async def test_paging_keeps_applied_url_and_does_not_apply_draft_search(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await connect_at(page, rust_backend, "?panel=keep")
            await expect(page.locator(".item-card")).to_have_count(50)
            applied_url = page.url
            await page.get_by_label("Search", exact=True).fill("not submitted")
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(100)
            assert page.url == applied_url
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(50)
            assert page.url == applied_url
        finally:
            await browser.close()


async def test_item_deep_link_outside_filter_and_first_page(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            query = urlencode(
                {"closet_brand": "Label A", "closet_item": "source-example-105"}
            )
            await connect_at(page, rust_backend, "?" + query)
            dialog = page.get_by_role("dialog", name="Example piece 105", exact=True)
            await expect(dialog).to_be_visible()
            await expect(page.locator(".item-card")).to_have_count(50)
            await expect(page.get_by_label("Brand", exact=True)).to_have_value(
                "Label A"
            )
            await page.reload()
            await reconnect(page, rust_backend)
            await expect(dialog).to_be_visible()
            await dialog.get_by_role("button", name="Close item details").click()
            await expect(dialog).to_be_hidden()
            assert parse_qs(urlsplit(page.url).query) == {"closet_brand": ["Label A"]}
            await page.locator(".item-card .item-open").first.click()
            await expect(page.get_by_role("dialog")).to_be_visible()
            item_url = page.url
            assert "closet_item=" in item_url
            await page.go_back()
            await expect(page.get_by_role("dialog")).to_be_hidden()
            await page.go_forward()
            await expect(page.get_by_role("dialog")).to_be_visible()
            assert page.url == item_url
            await page.keyboard.press("Escape")
            await expect(page.get_by_role("dialog")).to_be_hidden()
            assert "closet_item=" not in page.url
        finally:
            await browser.close()


@pytest.mark.parametrize("archived", [False, True])
async def test_missing_or_archived_deep_item_is_recoverable(
    rust_backend: RustBackend, archived: bool
) -> None:
    records = [catalog_item("example-current"), catalog_item("example-removed")]
    await rust_backend.sync(records)
    await rust_backend.sync(records[:1], key="archive-example")
    item_id = "example-removed" if archived else "example-never-existed"
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await connect_at(page, rust_backend, "?closet_item=" + item_id)
            dialog = page.get_by_role("dialog", name="Item unavailable", exact=True)
            await expect(dialog).to_be_visible()
            await dialog.get_by_role("button", name="Close item details").click()
            await expect(dialog).to_be_hidden()
            await expect(page.locator(".item-card")).to_have_count(1)
            assert "closet_item=" not in page.url
        finally:
            await browser.close()


async def test_back_and_new_item_ignore_a_late_detail_response(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(
        [
            catalog_item("example-slow", name="A slow jacket"),
            catalog_item("example-fast", name="B fast jacket"),
        ]
    )
    started = asyncio.Event()
    release = asyncio.Event()

    async def delayed(route: Route) -> None:
        response = await route.fetch()
        started.set()
        await release.wait()
        try:
            await route.fulfill(response=response)
        except Error:
            # Back aborts the browser request; the real backend reply was held.
            pass

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("**/v1/items/example-slow", delayed)
            await connect_at(page, rust_backend)
            await page.get_by_role("button", name="View A slow jacket").click()
            async with asyncio.timeout(5):
                await started.wait()
            await page.go_back()
            await expect(page.get_by_role("dialog")).to_be_hidden()
            await page.get_by_role("button", name="View B fast jacket").click()
            await expect(
                page.get_by_role("dialog", name="B fast jacket")
            ).to_be_visible()
            release.set()
            await page.wait_for_timeout(150)
            await expect(
                page.get_by_role("dialog", name="B fast jacket")
            ).to_be_visible()
            assert parse_qs(urlsplit(page.url).query)["closet_item"] == ["example-fast"]
        finally:
            release.set()
            await browser.close()


@pytest.mark.parametrize("width", [390, 1100])
async def test_gallery_switches_photos_with_keyboard_and_survives_a_failed_image(
    rust_backend: RustBackend, width: int
) -> None:
    item = catalog_item(name="Example three-view jacket")
    item["images"] = [
        {
            "url": f"https://images.example.invalid/{view}.svg",
            "source_url": f"https://example.invalid/{view}",
            "kind": "official_product_reference",
            "confidence": "high",
        }
        for view in ["front", "back", "broken"]
    ]
    await rust_backend.sync([item])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.route("https://images.example.invalid/**", product_photo)
            await page.route(
                "https://images.example.invalid/broken.svg", lambda route: route.abort()
            )
            await connect_at(page, rust_backend)
            await page.get_by_role(
                "button", name="View Example three-view jacket"
            ).click()
            dialog = page.get_by_role("dialog")
            counter = dialog.locator(".gallery-counter")
            await expect(counter).to_have_text("1 of 3")
            await dialog.get_by_role("button", name="Next photo", exact=True).click()
            await expect(counter).to_have_text("2 of 3")
            await expect(dialog.locator(".gallery-main img")).to_have_attribute(
                "src", "https://images.example.invalid/back.svg"
            )
            await expect(
                dialog.get_by_role("link", name="View product")
            ).to_have_attribute("href", "https://example.invalid/back")
            thumbnail = dialog.get_by_role("button", name="Show photo 2", exact=True)
            await thumbnail.focus()
            await page.keyboard.press("ArrowRight")
            await expect(counter).to_have_text("3 of 3")
            await expect(
                dialog.locator(".gallery-main").get_by_text("No photo", exact=True)
            ).to_be_visible()
            await expect(
                dialog.get_by_role("button", name="Show photo 3", exact=True)
            ).to_be_focused()
            await page.keyboard.press("Home")
            await expect(counter).to_have_text("1 of 3")
            await page.keyboard.press("End")
            await expect(counter).to_have_text("3 of 3")
            await dialog.get_by_role("button", name="Next photo", exact=True).click()
            await expect(counter).to_have_text("1 of 3")
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            await dialog.get_by_role("button", name="Close item details").click()
            await page.get_by_role(
                "button", name="View Example three-view jacket"
            ).click()
            await expect(counter).to_have_text("1 of 3")
        finally:
            await browser.close()


@pytest.mark.parametrize("mode", ["missing", "single", "failed"])
async def test_single_or_missing_photo_has_no_redundant_gallery_controls(
    rust_backend: RustBackend, mode: str
) -> None:
    item = catalog_item()
    if mode != "missing":
        item["images"] = [
            {
                "url": "https://images.example.invalid/one.svg",
                "kind": "official_product_reference",
                "confidence": "high",
            }
        ]
    await rust_backend.sync([item])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            if mode == "failed":
                await page.route(
                    "https://images.example.invalid/**", lambda route: route.abort()
                )
            else:
                await page.route("https://images.example.invalid/**", product_photo)
            await connect_at(page, rust_backend)
            await page.get_by_role("button", name="View Example cotton tee").click()
            dialog = page.get_by_role("dialog")
            await expect(dialog).to_be_visible()
            await expect(dialog.get_by_role("button", name="Next photo")).to_have_count(
                0
            )
            await expect(
                dialog.locator(".gallery-counter, .photo-thumbnail")
            ).to_have_count(0)
            if mode != "single":
                await expect(dialog.get_by_text("No photo", exact=True)).to_be_visible()
                await expect(
                    dialog.get_by_role("link", name="Open photo")
                ).to_have_count(0)
        finally:
            await browser.close()
