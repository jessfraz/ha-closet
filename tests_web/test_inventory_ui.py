"""Native scroll loading, compact source colors and visible failure recovery."""

import asyncio
import re

import pytest
from playwright.async_api import Route, async_playwright, expect

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_ha.tracking_fixture import configure
from tests_web.test_app import login
from tests_web.test_sources import source_records


async def product_photo(route: Route) -> None:
    """A local fictional image, never a household or retailer photograph."""
    await route.fulfill(
        content_type="image/svg+xml",
        body='<svg xmlns="http://www.w3.org/2000/svg" width="200" height="250">'
        '<rect width="200" height="250" fill="#e2e5e8"/>'
        '<path d="M60 50h80l-10 150H70z" fill="#3b4c61"/></svg>',
    )


@pytest.mark.parametrize(
    ("width", "theme"),
    [(320, "dark"), (390, "light"), (390, "dark"), (1100, "light")],
)
async def test_cards_read_image_brand_name_then_source_without_a_status_gap(
    rust_backend: RustBackend,
    width: int,
    theme: str,
) -> None:
    short = catalog_item("example-short-card", name="Denim shorts")
    short.update(brand="Example denim", size=None, color=None)
    long = catalog_item(
        "example-long-card",
        name="Relaxed wool overshirt with contrasting buttons and a curved hem",
    )
    long.update(brand="Example denim", size="M", color="Charcoal")
    long["shop"] = {
        "id": "example-atelier",
        "label": "Example atelier with a particularly descriptive archive label",
        "season": None,
        "years": [],
    }
    for record in (short, long):
        record["images"] = [
            {
                "url": "https://images.example.invalid/garment.svg",
                "kind": "retailer_product_reference",
                "confidence": "high",
            }
        ]
    await rust_backend.sync([short, long])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900},
                color_scheme="dark" if theme == "dark" else "light",
            )
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend)
            cards = page.locator(".item-card")
            await expect(cards).to_have_count(2)
            for card in await cards.all():
                await expect(card.locator(".tracking-pill")).to_be_visible()
                photo = await card.locator(".photo").bounding_box()
                brand = await card.locator(".brand-badge").bounding_box()
                name = await card.locator(".item-name").bounding_box()
                source = await card.locator(".source-badge").bounding_box()
                box = await card.bounding_box()
                assert photo and brand and name and source and box
                assert photo["y"] + photo["height"] <= brand["y"]
                assert brand["y"] + brand["height"] <= name["y"]
                assert name["y"] + name["height"] <= source["y"]
                gap = await card.locator(".source-badge").evaluate("""badge => {
                    const holder = badge.parentElement;
                    const prior = holder.previousElementSibling;
                    return holder.getBoundingClientRect().top
                        - prior.getBoundingClientRect().bottom;
                }""")
                assert abs(gap) <= 1, "source badge must follow its garment status"
                assert source["height"] >= 24 and brand["height"] >= 26
                assert (
                    await card.locator(".source-badge").evaluate(
                        "button => getComputedStyle(button).fontSize"
                    )
                    == "10.5px"
                )
                assert (
                    await card.locator(".brand-badge").evaluate(
                        "button => getComputedStyle(button).fontSize"
                    )
                    == "11px"
                )
                await expect(card.locator("button")).to_have_count(3)
                await expect(card.locator("button button, button a")).to_have_count(0)
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > window.innerWidth"
            )
            card = cards.first
            brand = card.locator(".brand-badge")
            detail = card.locator(".item-open")
            source = card.locator(".source-badge")
            await brand.focus()
            await page.keyboard.press("Tab")
            await expect(detail).to_be_focused()
            await page.keyboard.press("Tab")
            await expect(source).to_be_focused()
            await page.keyboard.press("Shift+Tab")
            await page.keyboard.press("Enter")
            await expect(page.get_by_role("dialog")).to_be_visible()
            await page.get_by_role("button", name="Close item details").click()
            await expect(detail).to_be_focused()
            await card.locator(".photo").scroll_into_view_if_needed()
            photo = await card.locator(".photo").bounding_box()
            assert photo
            await page.mouse.click(
                photo["x"] + photo["width"] / 2,
                photo["y"] + photo["height"] / 2,
            )
            await expect(page.get_by_role("dialog")).to_be_visible()
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_photo_provenance_is_not_ui_noise_but_specific_facts_survive(
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
) -> None:
    top = catalog_item("example-top", name="Example cotton shirt")
    top["color"] = "Charcoal"
    jeans = catalog_item("example-jeans", category="clothing.jeans")
    flats = catalog_item("example-flats", category="footwear")
    qualification = "Photo shows navy; the recorded garment is charcoal."
    for record in (top, jeans):
        record.update(size=None)
        record["images"] = [
            {
                "url": "https://images.example.invalid/garment.svg",
                "source_url": "https://example.invalid/product",
                "kind": "retailer_product_reference",
                "confidence": "medium",
                "notes": qualification if record is top else None,
            }
        ]
    await rust_backend.sync([top, jeans, flats])
    noise = re.compile(
        r"reference photo|reference image|product reference|qualified image reference",
        re.IGNORECASE,
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(3)
            assert not noise.search(await page.locator("#inventory").aria_snapshot())
            card = page.locator(".item-card").filter(
                has=page.get_by_role("button", name="View Example cotton shirt")
            )
            await expect(card.get_by_text(qualification, exact=True)).to_be_visible()
            await expect(card.locator(".item-open")).to_have_accessible_description(
                f"Example label · Charcoal · {qualification}"
            )
            await card.locator(".item-open").click()
            dialog = page.get_by_role("dialog")
            assert not noise.search(await dialog.aria_snapshot())
            await expect(dialog.get_by_text(qualification, exact=True)).to_be_visible()
            await expect(dialog.locator("dt").filter(has_text="Size")).to_have_count(0)
            await expect(
                dialog.locator("dt").filter(has_text="Quantity")
            ).to_have_count(1)
            await page.get_by_role("button", name="Close item details").click()
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_label("Occasion", exact=True).select_option("everyday")
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(page.locator(".pieces .item-open")).to_have_count(3)
            assert not noise.search(await page.locator("#today").aria_snapshot())
            await expect(
                page.locator(".pieces").get_by_text(qualification, exact=True)
            ).to_be_visible()
            today_top = page.locator(".pieces").get_by_role(
                "button", name="View Example cotton shirt", exact=True
            )
            await expect(today_top).to_have_accessible_description(
                f"Example label · {qualification}"
            )
            await today_top.click()
            assert not noise.search(await dialog.aria_snapshot())
            await expect(dialog.get_by_text(qualification, exact=True)).to_be_visible()
            assert ha_protocol.generations == 1
        finally:
            await browser.close()


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
