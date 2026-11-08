"""Use real catalog pages and accessible badges, never household fixtures."""

import asyncio
from urllib.parse import parse_qs, urlsplit

import pytest
from homeassistant.util.json import JsonObjectType
from playwright.async_api import Route, async_playwright, expect

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_web.test_app import login


def source_records() -> list[JsonObjectType]:
    """Each brand and one source span pages, with sources crossing brands."""
    records = []
    for index in range(106):
        item = catalog_item(
            f"source-example-{index:03}", name=f"Example piece {index:03}"
        )
        item["brand"] = "Label A" if index % 2 == 0 else "Label B"
        if index < 64:
            item["shop"] = {
                "id": "demo-atelier",
                "label": "Demo atelier",
                "season": None,
                "years": [],
            }
            item["purchase_source"] = "self_bought"
        elif index < 94:
            item["purchase_source"] = "self_bought"
        else:
            item["purchase_source"] = "Example gift"
        if index >= 103:
            item["category"] = "footwear"
        records.append(item)
    return records


async def test_badges_filter_the_full_catalog_and_form_can_combine_sources(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend, manual_paging=True)
            await expect(page.locator(".item-card")).to_have_count(50)
            await expect(page.locator(".item-card .source-badge")).to_have_count(50)
            assert set(
                await page.locator(".item-card .source-badge").all_text_contents()
            ) == {"Demo atelier", "Self-bought", "Example gift"}
            await expect(
                page.get_by_label("Source", exact=True)
                .get_by_role("option")
                .filter(has_text="Unspecified")
            ).to_have_count(0)
            await page.get_by_label("Category", exact=True).select_option(
                "clothing.top"
            )
            await page.get_by_label("Search", exact=True).fill("Example piece 000")
            await page.get_by_label("Brand", exact=True).select_option("Label A")
            await expect(page.locator(".item-card")).to_have_count(1)
            badge = page.get_by_role(
                "button", name="Show all items from Demo atelier", exact=True
            )
            await expect(badge).to_have_text("Demo atelier")
            await badge.focus()
            await page.keyboard.press("Enter")
            await expect(page.get_by_label("Source", exact=True)).to_have_value(
                "shop:demo-atelier"
            )
            await expect(page.get_by_label("Source", exact=True)).to_be_focused()
            await expect(page.get_by_label("Brand", exact=True)).to_have_value("")
            await expect(page.get_by_label("Category", exact=True)).to_have_value("")
            await expect(page.get_by_label("Search", exact=True)).to_have_value("")
            await expect(page.get_by_role("dialog")).to_be_hidden()
            await expect(page.locator("#inventory-count")).to_have_text(
                "64 items · 106 in inventory"
            )
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(64)
            await expect(
                page.get_by_role("button", name="Load more", exact=True)
            ).to_be_hidden()
            await page.get_by_role(
                "button", name="Show all items by Label A", exact=True
            ).first.click()
            await expect(page.get_by_label("Brand", exact=True)).to_be_focused()
            await expect(page.get_by_label("Source", exact=True)).to_have_value("")
            await expect(page.locator("#inventory-count")).to_have_text(
                "53 items · 106 in inventory"
            )
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(53)
            await page.get_by_label("Source", exact=True).select_option(
                "purchase:self_bought"
            )
            await page.get_by_label("Category", exact=True).select_option(
                "clothing.top"
            )
            await page.get_by_label("Search", exact=True).fill("Example piece")
            await page.get_by_role("button", name="Search", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(15)
            await expect(
                page.get_by_label("Source", exact=True).get_by_role("option")
            ).to_have_text(
                [
                    "All sources",
                    "Demo atelier (64)",
                    "Example gift (12)",
                    "Self-bought (30)",
                ]
            )
            await page.get_by_label("Source", exact=True).select_option(
                "purchase:Example gift"
            )
            await expect(page.locator(".item-card")).to_have_count(5)
            assert set(
                await page.locator(".item-card .source-badge").all_text_contents()
            ) == {"Example gift"}
            await page.get_by_label("Source", exact=True).select_option("")
            await expect(page.locator("#inventory-count")).to_have_text(
                "52 items · 106 in inventory"
            )
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(52)
            names = await page.locator(".item-card .item-name").all_text_contents()
            assert len(set(names)) == 52
        finally:
            await browser.close()


async def test_new_badge_filter_cancels_old_source_page_without_duplicate_cards(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    started = asyncio.Event()
    release = asyncio.Event()

    async def delayed_source(route: Route) -> None:
        response = await route.fetch()
        query = parse_qs(urlsplit(route.request.url).query)
        if query.get("source") == ["shop:demo-atelier"]:
            started.set()
            await release.wait()
        await route.fulfill(response=response)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend, manual_paging=True)
            await page.route("**/v1/items?**", delayed_source)
            await page.get_by_role(
                "button", name="Show all items from Demo atelier", exact=True
            ).first.click()
            async with asyncio.timeout(5):
                await started.wait()
            await page.get_by_role(
                "button", name="Show all items by Label A", exact=True
            ).first.click()
            await expect(page.locator("#inventory-count")).to_have_text(
                "53 items · 106 in inventory"
            )
            release.set()
            await page.wait_for_load_state("networkidle")
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(53)
            names = await page.locator(".item-card .item-name").all_text_contents()
            assert len(set(names)) == 53
            await expect(page.get_by_label("Source", exact=True)).to_have_value("")
        finally:
            release.set()
            await browser.close()


async def test_badges_preserve_exact_catalog_brand_and_source_values(
    rust_backend: RustBackend,
) -> None:
    """Filter keys are catalog identities, not text to trim or normalize."""
    spaced = catalog_item("source-spaced", name="Whitespace example")
    spaced["brand"] = "Label A "
    spaced["shop"] = {
        "id": "demo atelier ",
        "label": "Demo atelier",
        "season": None,
        "years": [],
    }
    plain = catalog_item("source-plain", name="Other example")
    plain["brand"] = "Label A"
    plain["shop"] = {
        "id": "demo atelier",
        "label": "Demo atelier",
        "season": None,
        "years": [],
    }
    await rust_backend.sync([spaced, plain])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            card = page.locator(".item-card").filter(
                has=page.get_by_role(
                    "button", name="View Whitespace example", exact=True
                )
            )
            await card.get_by_role(
                "button", name="Show all items by Label A", exact=True
            ).click()
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(
                page.get_by_role("button", name="View Whitespace example", exact=True)
            ).to_be_visible()
            await expect(page.get_by_label("Brand", exact=True)).to_have_value(
                "Label A "
            )
            await page.get_by_role(
                "button", name="Show all items from Demo atelier", exact=True
            ).click()
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(
                page.get_by_role("button", name="View Whitespace example", exact=True)
            ).to_be_visible()
            await expect(page.get_by_label("Source", exact=True)).to_have_value(
                "shop:demo atelier "
            )
        finally:
            await browser.close()


async def test_load_more_uses_applied_query_until_search_is_submitted(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend, manual_paging=True)
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.get_by_label("Search", exact=True).fill("Example piece 000")
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(100)
            await expect(page.locator("#inventory-count")).to_have_text("106 items")
            await expect(page.locator("#notice")).to_be_empty()
            await page.get_by_role("button", name="Search", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(
                page.get_by_role("button", name="Load more", exact=True)
            ).to_be_hidden()
            await expect(
                page.get_by_role("button", name="View Example piece 000", exact=True)
            ).to_be_visible()
        finally:
            await browser.close()


async def test_keyboard_paging_keeps_focus_until_the_last_page(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend, manual_paging=True)
            await expect(page.locator(".item-card")).to_have_count(50)
            more = page.get_by_role("button", name="Load more", exact=True)
            await more.focus()
            await page.keyboard.press("Enter")
            await expect(page.locator(".item-card")).to_have_count(100)
            await expect(more).to_be_focused()
            await page.keyboard.press("Enter")
            await expect(page.locator(".item-card")).to_have_count(106)
            await expect(more).to_be_hidden()
            await expect(
                page.locator(".item-card").nth(100).locator(".item-open")
            ).to_be_focused()
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_today_badges_return_to_inventory_without_another_generation(
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
) -> None:
    top = catalog_item("example-top")
    jeans = catalog_item("example-jeans", category="clothing.jeans")
    flats = catalog_item("example-flats", category="footwear")
    for record in (top, jeans):
        record["shop"] = {
            "id": "demo-atelier",
            "label": "Demo atelier",
            "season": None,
            "years": [],
        }
    flats["purchase_source"] = "self_bought"
    await rust_backend.sync([top, jeans, flats])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(page.locator(".pieces .item-open")).to_have_count(3)
            await (
                page.locator(".pieces")
                .get_by_role(
                    "button", name="Show all items from Demo atelier", exact=True
                )
                .first.click()
            )
            await expect(
                page.get_by_role("heading", name="Inventory", exact=True)
            ).to_be_visible()
            await expect(page.locator(".item-card")).to_have_count(2)
            await expect(page.get_by_label("Source", exact=True)).to_have_value(
                "shop:demo-atelier"
            )
            await expect(page.get_by_label("Source", exact=True)).to_be_focused()
            await page.get_by_role("button", name="Today", exact=True).click()
            await expect(page.locator(".pieces .item-open")).to_have_count(3)
            await (
                page.locator(".pieces")
                .get_by_role(
                    "button", name="Show all items by Example label", exact=True
                )
                .first.click()
            )
            await expect(page.locator(".item-card")).to_have_count(3)
            await expect(page.get_by_label("Source", exact=True)).to_have_value("")
            await expect(page.get_by_role("dialog")).to_be_hidden()
            await rust_backend.sync([flats], key="archive-demo-source")
            await page.get_by_role("button", name="Today", exact=True).click()
            await expect(page.locator("#today-result-label")).to_contain_text("Stale")
            await (
                page.locator(".pieces")
                .get_by_role(
                    "button", name="Show all items from Demo atelier", exact=True
                )
                .first.click()
            )
            await expect(page.locator(".item-card")).to_have_count(0)
            await expect(
                page.get_by_role("heading", name="No matching items", exact=True)
            ).to_be_visible()
            await expect(page.get_by_label("Source", exact=True)).to_have_value(
                "shop:demo-atelier"
            )
            await expect(
                page.get_by_label("Source", exact=True).get_by_role(
                    "option", name="Demo atelier (0)", exact=True
                )
            ).to_have_count(1)
            await page.get_by_label("Source", exact=True).select_option("")
            await expect(page.locator(".item-card")).to_have_count(1)
            assert ha_protocol.generations == 1
        finally:
            await browser.close()
