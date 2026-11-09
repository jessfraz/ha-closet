"""Readable labels retain stable category and acquisition identities."""

from urllib.parse import parse_qs, urlsplit

from playwright.async_api import async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import configure
from tests_web.test_app import login


async def test_category_labels_and_source_facts_keep_filter_bookmarks(
    rust_backend: RustBackend,
) -> None:
    record = catalog_item("example-shirt", name="Example linen shirt")
    record["category"] = "clothing.top.shirt"
    await rust_backend.sync([record])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            category = page.get_by_label("Category", exact=True)
            await category.select_option("clothing.top.shirt")
            await expect(category.locator("option:checked")).to_have_text(
                "Clothing / Top / Shirt"
            )
            await page.get_by_role("button", name="Search", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(1)
            assert parse_qs(urlsplit(page.url).query)["closet_category"] == [
                "clothing.top.shirt"
            ]
            await page.get_by_role(
                "button", name="View Example linen shirt", exact=True
            ).click()
            dialog = page.get_by_role("dialog")
            await expect(dialog.get_by_text("Source", exact=True)).to_be_visible()
            await expect(dialog.get_by_text("Self-bought", exact=True)).to_be_visible()
            await expect(
                dialog.get_by_text("Clothing / Top / Shirt", exact=True)
            ).to_be_visible()
            await expect(
                dialog.get_by_text("Purchase source", exact=True)
            ).to_have_count(0)
            await page.get_by_role("button", name="Close item details").click()
            bookmark = page.url
            await page.reload()
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await expect(page).to_have_url(bookmark)
            await expect(category.locator("option:checked")).to_have_text(
                "Clothing / Top / Shirt"
            )
            await page.get_by_role("button", name="Tags", exact=True).click()
            queue_category = page.get_by_label("Category", exact=True).last
            await expect(
                queue_category.locator('option[value="clothing.top.shirt"]')
            ).to_have_text("Clothing / Top / Shirt")
        finally:
            await browser.close()
