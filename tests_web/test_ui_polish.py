"""Readable native controls, compact photos and aligned outfit cards."""

import pytest
from playwright.async_api import async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import configure
from tests_web.test_app import login
from tests_web.test_inventory_ui import product_photo


@pytest.mark.parametrize("width", [320, 390, 1100])
async def test_filter_values_remain_readable_and_controls_align(
    rust_backend: RustBackend, width: int
) -> None:
    record = catalog_item()
    brand = "Example clothing label with an exceptionally descriptive name"
    record["brand"] = brand
    await rust_backend.sync([record])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await login(page, rust_backend)
            search = await page.get_by_label("Search", exact=True).bounding_box()
            select = page.get_by_label("Brand", exact=True)
            box = await select.bounding_box()
            assert search and box
            assert abs(search["height"] - box["height"]) <= 1
            if width == 1100:
                assert abs(search["y"] - box["y"]) <= 1
            await select.select_option(brand)
            await expect(
                select.locator("..").get_by_text(brand, exact=True).last
            ).to_be_visible()
            await page.get_by_role("button", name="Today", exact=True).click()
            weather = page.get_by_label("Weather", exact=True)
            box = await weather.bounding_box()
            assert box and box["width"] >= 170
            await weather.select_option("ignored")
            await expect(weather).to_have_value("ignored")
            await page.get_by_role("button", name="Tags", exact=True).click()
            category = page.get_by_label("Category", exact=True).last
            await expect(category).to_be_visible()
            box = await category.bounding_box()
            assert box and box["width"] >= 170
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


async def test_detail_uses_readable_source_compact_fallback_and_control_height(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="View Example cotton tee").click()
            detail = page.get_by_role("dialog")
            await expect(detail.get_by_text("Self-bought", exact=True)).to_be_visible()
            fallback = await detail.locator(".photo").bounding_box()
            assert fallback and fallback["height"] <= 160
            area = await detail.get_by_label("Confirm current area").bounding_box()
            baseline = await detail.get_by_role(
                "button", name="Set area baseline", exact=True
            ).bounding_box()
            assert area and baseline
            assert abs(area["height"] - baseline["height"]) <= 1
            assert abs(area["y"] - baseline["y"]) <= 1
        finally:
            await browser.close()


@pytest.mark.parametrize("width", [390, 1100])
async def test_enrollment_fallback_and_installation_photos_stay_compact(
    rust_backend: RustBackend, width: int
) -> None:
    plain = catalog_item("example-a", name="Example missing photo")
    pictured = catalog_item("example-b", name="Example pictured garment")
    pictured["images"] = [
        {
            "url": "https://images.example.invalid/garment.svg",
            "kind": "retailer_product_reference",
            "confidence": "high",
        }
    ]
    await rust_backend.sync([plain, pictured])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            fallback = await page.locator(".enroll-photo .photo").bounding_box()
            assert fallback and fallback["height"] <= 160
            await page.get_by_role(
                "button", name="Installation queue", exact=True
            ).click()
            await expect(page.locator(".tag-queue-card")).to_have_count(2)
            for photo in await page.locator(".tag-queue-card .photo").all():
                box = await photo.bounding_box()
                assert box and box["height"] <= 200
            image = page.locator(".tag-queue-card img")
            await expect(image).to_be_visible()
            assert await image.evaluate(
                "image => image.complete && image.naturalWidth > 0"
            )
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
@pytest.mark.parametrize("width", [320, 390, 1100])
async def test_outfit_badges_align_across_wrapped_names(
    rust_backend: RustBackend, width: int
) -> None:
    records = [
        catalog_item("example-top", name="Example tee"),
        catalog_item(
            "example-jeans",
            name="Example unusually descriptive relaxed denim trousers",
            category="clothing.jeans",
        ),
        catalog_item("example-flats", name="Example flats", category="footwear"),
    ]
    await rust_backend.sync(records)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_label("Occasion", exact=True).select_option("everyday")
            await page.get_by_role("button", name="Suggest outfits").click()
            cards = page.locator(".pieces .item-piece")
            await expect(cards).to_have_count(3)
            previous: tuple[float, float] | None = None
            for card in await cards.all():
                box = await card.bounding_box()
                badge = await card.locator(".source-badge").bounding_box()
                assert box and badge
                if previous and abs(previous[0] - box["y"]) <= 1:
                    assert abs(previous[1] - badge["y"]) <= 1
                previous = (box["y"], badge["y"])
        finally:
            await browser.close()
