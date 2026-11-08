"""Keep complete source labels usable in narrow Today columns."""

from typing import Literal

import pytest
from playwright.async_api import async_playwright, expect

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_web.test_app import login


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
@pytest.mark.parametrize("theme", ["light", "dark"])
@pytest.mark.parametrize("width", [320, 390, 430])
async def test_today_wrapped_source_labels_are_compact_and_follow_their_item(
    rust_backend: RustBackend, width: int, theme: Literal["light", "dark"]
) -> None:
    records = [
        catalog_item("example-top", name="Example cotton top with a longer name"),
        catalog_item("example-jeans", name="Example jeans", category="clothing.jeans"),
        catalog_item("example-flats", name="Example flats", category="footwear"),
    ]
    labels = [
        "Example spring collection",
        "Example winter collection 2026",
        "Example direct purchase",
    ]
    for number, (record, label) in enumerate(zip(records, labels, strict=True)):
        record["shop"] = {
            "id": f"demo-source-{number}",
            "label": label,
            "season": None,
            "years": [],
        }
    await rust_backend.sync(records)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900}, color_scheme=theme
            )
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_label("Occasion", exact=True).select_option("everyday")
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(page.locator(".pieces .source-badge")).to_have_count(3)
            for label in labels:
                badge = page.locator(".pieces").get_by_role(
                    "button", name=f"Show all items from {label}", exact=True
                )
                await expect(badge).to_have_text(label)
                await expect(badge).to_have_css("border-radius", "12px")
                assert await badge.evaluate(
                    "element => element.scrollWidth <= element.clientWidth"
                )
                gap = await badge.evaluate("""element => {
                    const holder=element.parentElement;
                    const current=holder.getBoundingClientRect();
                    const prior=holder.previousElementSibling.getBoundingClientRect();
                    return current.top-prior.bottom;
                }""")
                assert abs(gap) < 0.5, "source badge floats below its own item metadata"
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_bedtime_ui_sends_sleepwear_occasion_and_keeps_the_saved_result(
    rust_backend: RustBackend, ha_protocol: HAProtocol
) -> None:
    await rust_backend.sync(
        [
            catalog_item(
                "example-nightgown",
                name="Example nightgown",
                category="clothing.sleepwear.nightgown",
            ),
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            catalog_item("example-flats", category="footwear"),
        ]
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_label("Occasion", exact=True).select_option("bedtime")
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            assert ha_protocol.generations == 0
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Example bedtime combination")
            ).to_be_visible()
            await expect(page.locator(".pieces .item-open")).to_have_count(1)
            await expect(page.locator("#today-result-label")).to_contain_text("Bedtime")
            context = ha_protocol.last_context
            assert context is not None
            date = context["date"]
            assert isinstance(date, str)
            await expect(page.locator("#today-date")).to_contain_text(date)
            assert context["timezone"] == "UTC" and context["occasion"] == "bedtime"
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Example bedtime combination")
            ).to_be_visible()
            assert ha_protocol.generations == 1
        finally:
            await browser.close()
