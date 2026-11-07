"""Exercise Inventory and the optional Today view through the real app."""

import asyncio
from pathlib import Path

import pytest
from aiohttp import ClientSession
from playwright.async_api import Page, Request, Route, async_playwright, expect

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item


async def login(page: Page, backend: RustBackend) -> None:
    """Use the real standalone connection form, without storing a browser token."""
    await page.goto(backend.origin)
    await page.get_by_label("API token", exact=True).fill(backend.owner_token)
    await page.get_by_role("button", name="Connect", exact=True).click()
    await expect(
        page.get_by_role("heading", name="Inventory", exact=True)
    ).to_be_visible()


async def test_inventory_paging_filters_and_read_only_detail(
    rust_backend: RustBackend,
    tmp_path: Path,
) -> None:
    records = [
        catalog_item(f"example-{index:03}", name=f"Example piece {index:03}")
        for index in range(52)
    ]
    records[-1]["brand"] = "Second example label"
    records[-1]["notes"] = ["A fictional catalog note."]
    records[-1]["images"] = [
        {
            "url": "https://example.invalid/reference.jpg",
            "source_url": "https://example.invalid/product",
            "kind": "retailer_product_reference",
            "confidence": "medium",
            "notes": "Different-color reference.",
        }
    ]
    await rust_backend.sync(records)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://example.invalid/**", lambda route: route.abort())
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(50)
            await page.get_by_role("button", name="Load more", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(52)
            await expect(
                page.get_by_role("button", name="Load more", exact=True)
            ).to_be_hidden()
            await expect(
                page.get_by_role("button", name="View Example piece 050", exact=True)
            ).to_be_focused()
            await page.get_by_label("Brand", exact=True).select_option(
                "Second example label"
            )
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(
                page.get_by_text("Different-color reference.", exact=True)
            ).to_be_visible()
            card = page.locator(".item-card").first
            metadata = await card.locator(".item-meta").bounding_box()
            qualifiers = await card.locator(".qualifier").bounding_box()
            assert metadata is not None and qualifiers is not None
            assert qualifiers["y"] >= metadata["y"] + metadata["height"]
            await expect(
                page.get_by_role("button", name="View Example piece 051", exact=True)
            ).to_have_accessible_description(
                "Second example label · Size unknown · Cool white · "
                "Ownership unconfirmed · Reference photo · Different-color reference."
            )
            await page.get_by_role(
                "button", name="View Example piece 051", exact=True
            ).click()
            detail = page.get_by_role("dialog", name="Example piece 051")
            await expect(detail).to_be_visible()
            await expect(detail.get_by_text("Ownership unconfirmed")).to_have_count(0)
            await expect(detail.get_by_text("Unconfirmed", exact=True)).to_be_visible()
            await expect(
                detail.get_by_text("A fictional catalog note.", exact=True)
            ).to_be_visible()
            await expect(
                detail.get_by_text("Different-color reference.", exact=True)
            ).to_be_visible()
            await expect(
                detail.get_by_role("button", name="Edit", exact=True)
            ).to_have_count(0)
            await expect(detail.get_by_text("No photo", exact=True)).to_be_visible()
            await detail.get_by_role("button", name="Close item details").click()
            await page.get_by_label("Search", exact=True).fill(
                "no matching catalog item"
            )
            await page.get_by_role("button", name="Search", exact=True).click()
            await expect(
                page.get_by_role("heading", name="No matching items")
            ).to_be_visible()
            await page.screenshot(path=str(tmp_path / "inventory-search.png"))
        finally:
            await browser.close()


@pytest.mark.parametrize(
    ("width", "theme"), [(320, "dark"), (390, "light"), (1100, "light")]
)
async def test_shell_and_item_controls_fit_mobile_and_dark_mode(
    rust_backend: RustBackend,
    tmp_path: Path,
    width: int,
    theme: str,
) -> None:
    await rust_backend.sync([catalog_item(name="Example long-name cotton jersey tee")])
    async with ClientSession() as client:
        async with client.get(
            f"{rust_backend.origin}/v1/system",
            headers={"Authorization": f"Bearer {rust_backend.owner_token}"},
        ) as response:
            version = (await response.json())["version"]
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await page.emulate_media(
                color_scheme="dark" if theme == "dark" else "light"
            )
            errors: list[str] = []
            page.on("pageerror", lambda error: errors.append(str(error)))
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(1)
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > window.innerWidth"
            )
            assert await page.locator(".wordmark img:visible").evaluate(
                "image => image.complete && image.naturalWidth > 0"
            )
            for control in await page.locator(
                "button:visible, input:visible, select:visible"
            ).all():
                box = await control.bounding_box()
                assert box is not None
                assert box["height"] >= 44
                assert box["width"] >= 44
            await expect(page.locator("#app-version")).to_have_text(version)
            await expect(page.get_by_text("Connected", exact=True)).to_be_visible()
            await page.screenshot(
                path=str(tmp_path / f"inventory-{width}-{theme}.png"), full_page=True
            )
            assert not errors
        finally:
            await browser.close()


async def test_today_is_an_explicit_attempt_not_a_get_side_effect(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item(
                "example-jeans", name="Example jeans", category="clothing.jeans"
            ),
            catalog_item("example-flats", name="Example flats", category="footwear"),
        ]
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            posted: list[str] = []
            page.on(
                "request",
                lambda request: (
                    posted.append(request.url) if request.method == "POST" else None
                ),
            )
            await page.get_by_role("button", name="Today", exact=True).click()
            await expect(
                page.get_by_role("heading", name="No suggestions yet")
            ).to_be_visible()
            assert not posted
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(page.locator("#generation-state")).to_contain_text(
                "Today isn't configured"
            )
            assert len(posted) == 1
            async with ClientSession() as client:
                async with client.get(
                    f"{rust_backend.origin}/v1/today",
                    headers={"Authorization": f"Bearer {rust_backend.owner_token}"},
                ) as response:
                    result = await response.json()
            assert result["result"] is None
            assert result["generation"]["status"] == "failed"
            assert result["generation"]["inputs"]["weather_mode"] == "ignored"
            await page.get_by_role("button", name="Refresh", exact=True).click()
            assert len(posted) == 1
        finally:
            await browser.close()


async def test_failed_refresh_keeps_inventory_and_marks_connection_lost(
    rust_backend: RustBackend,
) -> None:
    """A transport failure is not a successful empty catalog or live connection."""
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await expect(page.locator(".item-card")).to_have_count(1)
            await page.route("**/v1/items?**", lambda route: route.abort())
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator("#notice")).to_contain_text("did not complete")
            await expect(page.locator("#notice")).to_contain_text(
                "previous inventory result"
            )
            await expect(page.locator("#service-status")).to_have_text(
                "Connection lost"
            )
            await expect(page.locator(".item-card")).to_have_count(1)
            await page.unroute("**/v1/items?**")
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator("#service-status")).to_have_text("Connected")
            await expect(page.locator("#notice")).to_be_empty()
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_successful_today_uses_real_generated_ids_and_keeps_result_inputs(
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
) -> None:
    """Browser -> Rust -> HA protocol -> Rust validation -> rendered result."""
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item(
                "example-jeans", name="Example jeans", category="clothing.jeans"
            ),
            catalog_item("example-flats", name="Example flats", category="footwear"),
        ]
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await expect(
                page.get_by_role("heading", name="No suggestions yet")
            ).to_be_visible()
            assert ha_protocol.generations == 0
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Example combination")
            ).to_be_visible()
            await expect(page.locator(".pieces .item-open")).to_have_count(3)
            await expect(page.locator("#weather")).to_contain_text("Last checked")
            await expect(page.locator("#weather")).to_contain_text("forecast samples")
            await expect(page.locator("#weather")).to_contain_text(
                "Partial forecast coverage"
            )
            assert ha_protocol.generations == 1
            assert ha_protocol.forecasts == 1
            await page.get_by_label("Occasion", exact=True).select_option("dinner")
            await expect(page.locator("#today-input-note")).to_be_visible()
            await expect(page.locator("#today-result-label")).to_contain_text(
                "Everyday"
            )
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Example combination")
            ).to_be_visible()
            assert ha_protocol.generations == 1
        finally:
            await browser.close()


@pytest.mark.parametrize("gateway_status", [502, 504])
@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_uncertain_gateway_reply_retries_the_accepted_generation(
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
    gateway_status: int,
) -> None:
    """A gateway failure after acceptance must not turn retry into a second bet."""
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            catalog_item("example-flats", category="footwear"),
        ]
    )
    attempts: list[Request] = []

    async def gateway(route: Route) -> None:
        attempts.append(route.request)
        accepted = await route.fetch()
        assert accepted.status == 202
        if len(attempts) == 1:
            await route.fulfill(
                status=gateway_status,
                content_type="text/html",
                body="The gateway did not receive an acknowledgement.",
            )
        else:
            await route.fulfill(response=accepted)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.route("**/v1/today/generate", gateway)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_label("Occasion", exact=True).select_option("dinner")
            await page.get_by_label("Weather", exact=True).select_option("ignored")
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            retry = page.get_by_role("button", name="Retry same request", exact=True)
            await expect(retry).to_be_visible()
            await expect(page.get_by_label("Occasion", exact=True)).to_be_disabled()
            await expect(page.get_by_label("Weather", exact=True)).to_be_disabled()
            await page.get_by_role("button", name="Inventory", exact=True).click()
            await page.get_by_role("button", name="Today", exact=True).click()
            await expect(page.locator("#generation-state")).to_contain_text(
                "not acknowledged"
            )
            await retry.click()
            await expect(
                page.get_by_role("heading", name="Example combination")
            ).to_be_visible()
            assert len(attempts) == 2
            assert (
                attempts[0].headers["idempotency-key"]
                == attempts[1].headers["idempotency-key"]
            )
            assert (
                attempts[0].post_data_json
                == attempts[1].post_data_json
                == {
                    "occasion": "dinner",
                    "ownership_scope": "all_inventory",
                    "weather_mode": "ignored",
                }
            )
            assert ha_protocol.generations == 1
        finally:
            await browser.close()


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_overlapping_today_reads_publish_only_the_latest_render(
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
) -> None:
    """A delayed item read from an older refresh cannot duplicate new cards."""
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            catalog_item("example-flats", category="footwear"),
        ]
    )
    started = asyncio.Event()
    release = asyncio.Event()

    async def delayed_item(route: Route) -> None:
        response = await route.fetch()
        if not started.is_set():
            started.set()
            await release.wait()
        await route.fulfill(response=response)

    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Today", exact=True).click()
            await page.get_by_role("button", name="Suggest outfits", exact=True).click()
            await expect(page.locator(".suggestion")).to_have_count(1)
            await page.route("**/v1/items/example-top", delayed_item)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            async with asyncio.timeout(5):
                await started.wait()
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator(".suggestion")).to_have_count(1)
            release.set()
            await page.wait_for_load_state("networkidle")
            await expect(page.locator(".suggestion")).to_have_count(1)
            assert ha_protocol.generations == 1
        finally:
            release.set()
            await browser.close()
