"""All occasions remain one explicit request, with independently safe groups."""

import pytest
from playwright.async_api import async_playwright, expect

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_web.test_app import login


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_all_is_the_default_and_one_request_renders_each_eligible_group(
    rust_backend: RustBackend, ha_protocol: HAProtocol
) -> None:
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            catalog_item("example-flats", category="footwear"),
            catalog_item("example-nightgown", category="clothing.sleepwear.nightgown"),
        ]
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await expect(page.get_by_label("Occasion", exact=True)).to_have_value("all")
            assert ha_protocol.generations == 0
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            for group in ["Everyday", "Work", "Evening", "Bedtime"]:
                await expect(
                    page.get_by_role("heading", name=group, exact=True)
                ).to_be_visible()
            await expect(page.locator(".suggestion")).to_have_count(4)
            await expect(
                page.locator(".today-group").last.locator(".item-open")
            ).to_have_count(1)
            assert ha_protocol.generations == 1
            assert ha_protocol.last_context is not None
            assert ha_protocol.last_context["occasion"] == "all"
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator(".today-group")).to_have_count(4)
            assert ha_protocol.generations == 1
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()
