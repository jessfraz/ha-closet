"""Readable native controls, compact photos and aligned outfit cards."""

import pytest
from playwright.async_api import Locator, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import command, configure, snapshot
from tests_web.test_app import login
from tests_web.test_group_detail import prepare_group
from tests_web.test_inventory_ui import product_photo


async def assert_mobile_search_action(form: Locator) -> None:
    """A mobile search action gets its own row after every filter field."""
    bounds = await form.bounding_box()
    action = await form.get_by_role("button").bounding_box()
    assert bounds and action
    assert abs(action["x"] - bounds["x"]) <= 1
    assert abs(action["width"] - bounds["width"]) <= 1
    for control in await form.locator("input:visible, select:visible").all():
        box = await control.bounding_box()
        assert box and action["y"] >= box["y"] + box["height"] + 11


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
            else:
                await assert_mobile_search_action(page.locator("#filters"))
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
            if width < 620:
                await assert_mobile_search_action(
                    page.locator("#tag-workspace .filters")
                )
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


@pytest.mark.parametrize("width", [320, 390, 1440])
async def test_copy_source_rows_keep_the_same_layout_in_cards_and_details(
    rust_backend: RustBackend, width: int
) -> None:
    await prepare_group(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend, manual_paging=True)
            group = page.locator('[data-variant-id="example-variant"]')
            await expect(group.locator(".item-copy")).to_have_count(3)
            dialog = page.get_by_role("dialog", name="Example grouped tee")
            for expanded in (False, True):
                root = dialog if expanded else group
                if expanded:
                    await group.get_by_role(
                        "button", name="View all copies of Example grouped tee"
                    ).click()
                await expect(root.locator(".copy-actions")).to_have_count(3)
                for row in await root.locator(".copy-actions").all():
                    action = await row.locator(".copy-open").bounding_box()
                    source = await row.locator(".source-badge").bounding_box()
                    assert action and source
                    assert source["y"] >= action["y"] + action["height"]
                    assert abs(source["x"] - action["x"]) <= 1
            await dialog.locator('[data-copy-id="example-998"] .copy-open').click()
            await expect(page).to_have_url(
                f"{rust_backend.origin}/?closet_item=example-998"
            )
            await expect(dialog.get_by_text("$64.00", exact=True)).to_be_visible()
            await expect(dialog.get_by_text("S", exact=True)).to_be_visible()
            await dialog.get_by_role("button", name="‹ All copies").click()
            await expect(dialog.locator(".item-copy")).to_have_count(3)
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


@pytest.mark.parametrize(
    ("width", "wide_font", "long_area"),
    [
        (1280, False, False),
        (1280, True, False),
        (760, True, False),
        (390, True, False),
        (1280, False, True),
    ],
)
async def test_detail_uses_readable_source_compact_fallback_and_control_height(
    rust_backend: RustBackend, width: int, wide_font: bool, long_area: bool
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    area_label = "Example closet with an exceptionally descriptive area label"
    if long_area:
        config = (await snapshot(rust_backend))["config"]
        assert isinstance(config, dict)
        areas = config["areas"]
        assert isinstance(areas, list) and isinstance(areas[0], dict)
        areas[0]["label"] = area_label
        await command(rust_backend, {"type": "configure", "config": config})
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            if wide_font:
                # Linux's system font needs more room for this action than the
                # macOS font. Exercise the same constraint on either platform.
                async def wider_controls(route: Route) -> None:
                    response = await route.fetch()
                    stylesheet = await response.text()
                    await route.fulfill(
                        response=response,
                        body=stylesheet
                        + "\n.tracking-form button { font-family: monospace; }",
                    )

                await page.route("**/style.css", wider_controls)
            await login(page, rust_backend)
            await page.get_by_role("button", name="View Example cotton tee").click()
            detail = page.get_by_role("dialog")
            await expect(detail.get_by_text("Self-bought", exact=True)).to_be_visible()
            fallback = await detail.locator(".photo").bounding_box()
            assert fallback and fallback["height"] <= 160
            selector = detail.get_by_label("Confirm current area")
            if long_area:
                await expect(
                    selector.locator("..").get_by_text(area_label, exact=True).last
                ).to_be_visible()
            area = await selector.bounding_box()
            field = await selector.locator("..").bounding_box()
            baseline = await detail.get_by_role(
                "button", name="Set area baseline", exact=True
            ).bounding_box()
            assert area and field and baseline
            assert area["height"] == baseline["height"] == 44
            assert abs(baseline["x"] - area["x"]) <= 1
            assert baseline["y"] >= field["y"] + field["height"] + 11
            text = await detail.get_by_role(
                "button", name="Set area baseline", exact=True
            ).evaluate("""button => {
                const range = document.createRange();
                range.selectNodeContents(button);
                const text = range.getBoundingClientRect();
                const bounds = button.getBoundingClientRect();
                return {
                    lines: range.getClientRects().length,
                    left: text.left - bounds.left,
                    right: bounds.right - text.right,
                    top: text.top - bounds.top,
                    bottom: bounds.bottom - text.bottom,
                };
            }""")
            assert text["lines"] == 1
            assert min(text["left"], text["right"], text["top"], text["bottom"]) >= 0
        finally:
            await browser.close()


@pytest.mark.parametrize("width", [390, 1440])
async def test_single_purchase_group_status_remains_compact(
    rust_backend: RustBackend, width: int
) -> None:
    record = catalog_item("example-pullover", name="Example long-sleeve cotton layer")
    record["identity"] = {
        "style_id": "example-pullover-style",
        "variant_id": "example-pullover-variant",
    }
    await rust_backend.sync([record])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await login(page, rust_backend)
            pill = page.locator(
                '[data-variant-id="example-pullover-variant"] > .tracking-pill'
            )
            await expect(pill).to_have_text("Not tracked")
            extra_width = await pill.evaluate("""pill => {
                const range = document.createRange();
                range.selectNodeContents(pill);
                return pill.getBoundingClientRect().width
                    - range.getBoundingClientRect().width;
            }""")
            assert 0 <= extra_width <= 17, "a status pill should fit its own text"
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
