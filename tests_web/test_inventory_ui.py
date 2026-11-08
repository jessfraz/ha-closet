"""Native scroll loading, compact source colors and visible failure recovery."""

import asyncio

import pytest
from playwright.async_api import Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_web.test_app import login
from tests_web.test_sources import source_records


async def test_scrolling_loads_pages_without_moving_focus_or_duplicating_items(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            requests: list[str] = []
            page.on(
                "request",
                lambda request: (
                    requests.append(request.url)
                    if "/v1/items?" in request.url
                    else None
                ),
            )
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(50)
            focused = page.locator(".item-card .item-open").first
            await focused.focus()
            await page.mouse.wheel(0, 15000)
            await expect(page.locator(".item-card")).to_have_count(100)
            await expect(focused).to_be_focused()
            await page.mouse.wheel(0, 15000)
            await expect(page.locator(".item-card")).to_have_count(106)
            await expect(focused).to_be_focused()
            await expect(
                page.get_by_role("button", name="Load more", exact=True)
            ).to_be_hidden()
            await page.mouse.wheel(0, 15000)
            await page.wait_for_load_state("networkidle")
            assert len(requests) == 3
            names = await page.locator(".item-card .item-name").all_text_contents()
            assert len(set(names)) == 106
        finally:
            await browser.close()


async def test_filter_change_cancels_an_automatic_append(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    started = asyncio.Event()
    release = asyncio.Event()

    async def delayed_page(route: Route) -> None:
        response = await route.fetch()
        if "page_token=" in route.request.url and not started.is_set():
            started.set()
            await release.wait()
        await route.fulfill(response=response)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.route("**/v1/items?**", delayed_page)
            await page.mouse.wheel(0, 15000)
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
            await page.mouse.wheel(0, 15000)
            await expect(page.locator(".item-card")).to_have_count(53)
            names = await page.locator(".item-card .item-name").all_text_contents()
            assert len(set(names)) == 53
            assert set(await page.locator(".brand-badge").all_text_contents()) == {
                "Label A"
            }
        finally:
            release.set()
            await browser.close()


async def test_failed_automatic_page_pauses_until_manual_retry(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(source_records())
    attempts = 0

    async def failed_page(route: Route) -> None:
        nonlocal attempts
        if "page_token=" in route.request.url:
            attempts += 1
            await route.abort()
        else:
            await route.continue_()

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.route("**/v1/items?**", failed_page)
            await page.mouse.wheel(0, 15000)
            await expect(page.locator("#notice")).to_contain_text("Use Load more")
            await page.wait_for_load_state("networkidle")
            assert attempts == 1
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.unroute("**/v1/items?**", failed_page)
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card").nth(99)).to_be_attached()
            await page.wait_for_load_state("networkidle")
            await expect(page.locator("#notice")).to_be_empty()
            names = await page.locator(".item-card .item-name").all_text_contents()
            assert len(names) in (100, 106)
            assert len(set(names)) == len(names)
        finally:
            await browser.close()


def luminance(value: str) -> float:
    channels = [
        int(part) / 255
        for part in value.removeprefix("rgb(").removesuffix(")").split(",")
    ]
    linear = [
        channel / 12.92 if channel <= 0.04045 else ((channel + 0.055) / 1.055) ** 2.4
        for channel in channels
    ]
    return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]


@pytest.mark.parametrize("theme", ["light", "dark"])
async def test_source_badges_are_compact_stable_and_readable(
    rust_backend: RustBackend,
    theme: str,
) -> None:
    records = []
    for index in range(12):
        for suffix in ("a", "b"):
            record = catalog_item(f"color-example-{index:02}-{suffix}")
            record["shop"] = {
                "id": f"demo-color-{index}",
                "label": f"Example source {index:02}",
                "season": None,
                "years": [],
            }
            records.append(record)
    await rust_backend.sync(records)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                color_scheme="dark" if theme == "dark" else "light"
            )
            await login(page, rust_backend)
            await expect(page.locator(".source-badge")).to_have_count(24)
            colors: dict[str, tuple[str, str]] = {}
            for badge in await page.locator(".source-badge").all():
                label = await badge.inner_text()
                style = await badge.evaluate(
                    """button => {
                        const style = getComputedStyle(button);
                        return {
                            foreground: style.color,
                            background: style.backgroundColor
                        };
                    }"""
                )
                pair = (style["foreground"], style["background"])
                assert label.startswith("Example source ")
                if label in colors:
                    assert colors[label] == pair
                colors[label] = pair
                light, dark = sorted(
                    (luminance(pair[0]), luminance(pair[1])), reverse=True
                )
                assert (light + 0.05) / (dark + 0.05) >= 4.5
                box = await badge.bounding_box()
                assert box is not None and 24 <= box["height"] <= 32
            assert len(set(colors.values())) >= 3
            await page.get_by_role(
                "button", name="Show all items from Example source 00", exact=True
            ).first.click()
            await expect(page.locator(".source-badge")).to_have_count(2)
            for badge in await page.locator(".source-badge").all():
                style = await badge.evaluate(
                    """button => {
                        const style = getComputedStyle(button);
                        return {
                            foreground: style.color,
                            background: style.backgroundColor
                        };
                    }"""
                )
                assert (style["foreground"], style["background"]) == colors[
                    "Example source 00"
                ]
        finally:
            await browser.close()
