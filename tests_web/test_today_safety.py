"""Safety footwear is deliberate, cached by mode and never a casual fallback."""

import pytest
from playwright.async_api import async_playwright, expect

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_ha.tracking_fixture import command, snapshot, units
from tests_web.test_app import login


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
@pytest.mark.parametrize("width", [320, 390])
async def test_safety_checkbox_is_explicit_restored_and_unavailable_for_bedtime(
    rust_backend: RustBackend, ha_protocol: HAProtocol, width: int
) -> None:
    footwear = catalog_item(
        "example-flats", name="Example safety boot", category="footwear.boot"
    )
    footwear["usage"] = {"kind": "safety_work_only", "requirement": "steel_toe"}
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            footwear,
            catalog_item("example-nightgown", category="clothing.sleepwear.nightgown"),
        ]
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 844}, is_mobile=True, has_touch=True
            )
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).tap()
            safety = page.get_by_role("checkbox", name="Steel-toe footwear required")
            await expect(safety).not_to_be_checked()
            await expect(safety).to_be_enabled()
            option = page.locator('label[for="steel-toe"]')
            label_box = await option.bounding_box()
            check_box = await safety.bounding_box()
            suggest_box = await page.locator("#suggest").bounding_box()
            assert label_box and check_box and suggest_box
            assert check_box["width"] == 17
            assert 44 <= label_box["height"] <= 60
            assert label_box["y"] + label_box["height"] <= suggest_box["y"]
            assert await option.evaluate(
                "label => label.scrollWidth <= label.clientWidth"
            )
            await safety.check()
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            async with page.expect_request(
                lambda request: (
                    request.method == "POST"
                    and request.url.endswith("/v1/today/generate")
                )
            ) as generated:
                await page.get_by_role("button", name="Suggest outfits").tap()
            assert (await generated.value).post_data_json == {
                "occasion": "all",
                "weather_mode": "ignored",
                "footwear_requirement": "steel_toe",
            }
            await expect(page.locator(".today-group")).to_have_count(3)
            await expect(page.locator("#today-result-label")).to_contain_text(
                "Steel-toe footwear required"
            )
            await expect(
                page.get_by_role("heading", name="Bedtime", exact=True)
            ).to_have_count(0)
            await expect(page.locator("#today .usage-badge")).to_have_count(3)
            await page.get_by_role("button", name="Refresh", exact=True).tap()
            await expect(safety).to_be_checked()
            await expect(page.locator(".today-group")).to_have_count(3)
            assert ha_protocol.generations == 1
            await page.get_by_label("Occasion", exact=True).select_option("bedtime")
            await expect(safety).not_to_be_checked()
            await expect(safety).to_be_disabled()
            await expect(page.locator(".today-group")).to_have_count(0)
            await page.get_by_label("Occasion", exact=True).select_option("work")
            await expect(safety).to_be_enabled()
            await expect(safety).not_to_be_checked()
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_no_eligible_safety_footwear_has_no_fallback_and_off_omits_requirement(
    rust_backend: RustBackend, ha_protocol: HAProtocol
) -> None:
    await rust_backend.sync(
        [
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
            await page.get_by_label("Occasion", exact=True).select_option("everyday")
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            safety = page.get_by_role("checkbox", name="Steel-toe footwear required")
            await safety.check()
            await page.get_by_role("button", name="Suggest outfits").click()
            await expect(page.locator("#generation-state")).to_contain_text(
                "The last attempt found no available steel-toe footwear. "
                "Try again after availability changes."
            )
            await expect(page.locator(".suggestion")).to_have_count(0)
            assert ha_protocol.generations == 0
            await safety.uncheck()
            await expect(page.locator("#generation-state")).to_be_hidden()
            async with page.expect_request(
                lambda request: (
                    request.method == "POST"
                    and request.url.endswith("/v1/today/generate")
                )
            ) as generated:
                await page.get_by_role("button", name="Suggest outfits").click()
            assert (await generated.value).post_data_json == {
                "occasion": "everyday",
                "weather_mode": "ignored",
            }
            await expect(page.locator(".suggestion")).to_have_count(1)
            assert ha_protocol.generations == 1
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_failed_footwear_attempt_stays_historical_after_availability_changes(
    rust_backend: RustBackend, ha_protocol: HAProtocol
) -> None:
    boot = catalog_item(
        "example-flats", name="Example safety boot", category="footwear.boot"
    )
    boot["usage"] = {"kind": "safety_work_only", "requirement": "steel_toe"}
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            boot,
        ]
    )
    physical = units(await snapshot(rust_backend))
    boot_unit = next(unit for unit in physical if unit["catalog_id"] == boot["id"])
    await command(
        rust_backend,
        {
            "type": "correct",
            "unit_id": boot_unit["id"],
            "status": "in_wash",
            "note": "Fictional unavailable footwear.",
        },
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_label("Occasion", exact=True).select_option("everyday")
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            await page.get_by_label("Steel-toe footwear required", exact=True).check()
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            message = page.locator("#generation-state")
            await expect(message).to_have_text(
                "The last attempt found no available steel-toe footwear. "
                "Try again after availability changes."
            )
            assert ha_protocol.generations == 0
            await command(
                rust_backend,
                {
                    "type": "correct",
                    "unit_id": boot_unit["id"],
                    "status": "clean_ready",
                    "note": "Fictional restored footwear.",
                },
            )
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(message).to_have_text(
                "The last attempt found no available steel-toe footwear. "
                "Try again after availability changes."
            )
            assert ha_protocol.generations == 0
            await expect(page.locator(".suggestion")).to_have_count(0)
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(page.locator(".suggestion")).to_have_count(1)
            assert ha_protocol.generations == 1
        finally:
            await browser.close()
