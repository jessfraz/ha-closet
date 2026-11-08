"""Trusted touch filtering must not activate the native select control."""

import pytest
from playwright.async_api import async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_app import login


@pytest.mark.parametrize("width", [320, 390, 430])
async def test_touch_badges_filter_the_catalog_without_focusing_native_selects(
    rust_backend: RustBackend, width: int
) -> None:
    top = catalog_item("touch-top", name="Example top")
    pants = catalog_item("touch-pants", name="Example pants", category="clothing.pants")
    shoes = catalog_item("touch-shoes", name="Example shoes", category="footwear")
    for row in [top, pants]:
        row["brand"] = "First example label"
    shoes["brand"] = "Second example label"
    for row in [top, shoes]:
        row["shop"] = {
            "id": "demo-atelier",
            "label": "Example atelier",
            "season": None,
            "years": [],
        }
    await rust_backend.sync([top, pants, shoes])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900}, is_mobile=True, has_touch=True
            )
            await login(page, rust_backend)
            await page.get_by_label("Search", exact=True).fill("Example top")
            await page.get_by_label("Category", exact=True).select_option(
                "clothing.top"
            )
            await page.get_by_label("Source", exact=True).select_option(
                "shop:demo-atelier"
            )
            await page.get_by_role("button", name="Search", exact=True).tap()
            await expect(page.locator(".item-card")).to_have_count(1)
            await page.get_by_role(
                "button", name="Show all items by First example label", exact=True
            ).tap()
            await expect(page.locator(".item-card")).to_have_count(2)
            brand = page.get_by_label("Brand", exact=True)
            await expect(brand).to_have_value("First example label")
            await expect(brand).not_to_be_focused()
            for field in ["Search", "Category", "Source"]:
                await expect(page.get_by_label(field, exact=True)).to_have_value("")
            await expect(page.locator("#detail")).to_be_hidden()
            await page.get_by_role(
                "button", name="Show all items from Example atelier", exact=True
            ).tap()
            await expect(page.locator(".item-card")).to_have_count(2)
            source = page.get_by_label("Source", exact=True)
            await expect(source).to_have_value("shop:demo-atelier")
            await expect(source).not_to_be_focused()
            await expect(brand).to_have_value("")
            await expect(page.locator("#detail")).to_be_hidden()
            await expect(page.locator(".brand-badge")).to_have_text(
                ["First example label", "Second example label"]
            )
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()
