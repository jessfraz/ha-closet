"""Observed wear through the real analytics API and browser UI."""

import asyncio
import json
import os
import re
import sqlite3
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

import pytest
from aiohttp import ClientSession
from homeassistant.util.json import JsonObjectType
from playwright.async_api import Page, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import command


async def fictional_photo(route: Route) -> None:
    """Original vector fixture clothes, without retailer or household photos."""
    if "dress" in route.request.url:
        shape = (
            '<path d="M156 68l-46 42 20 51 25-16-35 255q80 25 160 0'
            'l-35-255 25 16 20-51-46-42-25 24h-38z" '
            'fill="#b7c3b3" stroke="#7e8c7b" stroke-width="2"/>'
            '<path d="M155 206h90M181 92q19 20 38 0" fill="none" '
            'stroke="#7e8c7b" stroke-width="2"/>'
        )
    elif "knit" in route.request.url:
        shape = (
            '<path d="M151 83l-54 32-24 183 37 8 35-124-4 209q60 14 120 0'
            'l-4-209 35 124 37-8-24-183-54-32q-50 37-100 0z" '
            'fill="#d1ceca" stroke="#a5a19c" stroke-width="2"/>'
            '<path d="M151 83q50 56 100 0M143 375h116M80 282l35 7M286 289l35-7" '
            'fill="none" stroke="#a5a19c" stroke-width="5"/>'
        )
    else:
        shape = (
            '<path d="M157 70l-57 35-35 187 37 11 44-134-4 227q58 18 116 0'
            'l-4-227 44 134 37-11-35-187-57-35-43 26z" '
            'fill="#9eafc1" stroke="#607c98" stroke-width="2"/>'
            '<path d="M200 96v307M157 70l-8 48 51-22 51 22-8-48'
            'M222 160h23v39h-23z" fill="none" stroke="#607c98" stroke-width="2"/>'
        )
    await route.fulfill(
        content_type="image/svg+xml",
        body=(
            '<svg xmlns="http://www.w3.org/2000/svg" width="400" height="500">'
            f'<rect width="400" height="500" fill="white"/>{shape}</svg>'
        ),
    )


async def seed_analytics(
    backend: RustBackend, temporary: Path, *, fixture: Path | None = None
) -> None:
    """Seed only a disposable DB with real reducer/exported historical bodies."""
    target = temporary / "analytics-history.json"
    if fixture is not None:
        await asyncio.to_thread(
            target.write_bytes, await asyncio.to_thread(fixture.read_bytes)
        )
    else:
        environment = os.environ.copy()
        environment["CLOSET_ANALYTICS_FIXTURE"] = str(target)
        process = await asyncio.create_subprocess_exec(
            "cargo",
            "test",
            "--locked",
            "--test",
            "tracking_analytics",
            "write_browser_fixture",
            "--",
            "--ignored",
            "--exact",
            cwd=Path(__file__).parents[1],
            env=environment,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        try:
            async with asyncio.timeout(120):
                output, _ = await process.communicate()
            assert process.returncode == 0, output.decode()
        finally:
            if process.returncode is None:
                process.kill()
                await process.wait()
    data: JsonObjectType = json.loads(await asyncio.to_thread(target.read_text))
    records = data["catalog"]
    tracking = data["tracking"]
    assert isinstance(records, list) and isinstance(tracking, dict)
    catalog: list[JsonObjectType] = []
    for record in records:
        assert isinstance(record, dict)
        identifier = record["id"]
        assert isinstance(identifier, str)
        record["images"] = [
            {
                "url": f"https://images.example.invalid/{identifier}.svg",
                "kind": "retailer_product_reference",
                "confidence": "high",
            }
        ]
        catalog.append(record)
    await backend.sync(catalog)

    def install() -> None:
        # The running fixture owns this DB. The transaction and revision bump
        # let the real Store reload a coherent historical projection next read.
        with sqlite3.connect(temporary / "catalog.sqlite3") as connection:
            for table in [
                "tracking_history",
                "tracking_sessions",
                "tracking_bindings",
                "tracking_units",
                "tracking_coverage",
            ]:
                connection.execute(f"DELETE FROM {table}")
            configurations = tracking["configurations"]
            assert isinstance(configurations, list)
            for configuration in configurations:
                assert isinstance(configuration, dict)
                connection.execute(
                    "INSERT OR REPLACE INTO tracking_configurations(version,at,body) "
                    "VALUES(?,?,?)",
                    (
                        configuration["version"],
                        configuration["at"],
                        json.dumps(configuration["config"]),
                    ),
                )
            stored_units = tracking["units"]
            assert isinstance(stored_units, list)
            for stored in stored_units:
                assert isinstance(stored, dict)
                unit = stored["unit"]
                assert isinstance(unit, dict)
                connection.execute(
                    "INSERT INTO tracking_units(id,catalog_id,ordinal,epc,status,"
                    "last_confirmed_area,current_area,baseline_at,evidence,fence_at,"
                    "paused,retired,archived,exception,changed_at,last_event_id,body) "
                    "VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        unit["id"],
                        unit["catalog_id"],
                        unit["ordinal"],
                        stored["epc"],
                        unit["status"],
                        unit["last_confirmed_area"],
                        unit["current_area"],
                        unit["baseline_at"],
                        unit["evidence"],
                        unit["fence_at"],
                        unit["paused"],
                        unit["retired"],
                        unit["archived"],
                        unit["exception"],
                        unit["changed_at"],
                        unit["last_event_id"],
                        json.dumps(stored),
                    ),
                )
            history = tracking["history"]
            assert isinstance(history, list)
            for event in history:
                assert isinstance(event, dict)
                connection.execute(
                    "INSERT INTO tracking_history(id,unit_id,at,kind,body) "
                    "VALUES(?,?,?,?,?)",
                    (
                        event["id"],
                        event["unit_id"],
                        event["at"],
                        event["kind"],
                        json.dumps(event),
                    ),
                )
            bindings = tracking["bindings"]
            assert isinstance(bindings, list)
            for binding in bindings:
                assert isinstance(binding, dict)
                connection.execute(
                    "INSERT INTO tracking_bindings("
                    "id,unit_id,epc,bound_at,retired_at,retirement) "
                    "VALUES(?,?,?,?,?,?)",
                    (
                        binding["id"],
                        binding["unit_id"],
                        binding["epc"],
                        binding["bound_at"],
                        binding["retired_at"],
                        binding["retirement"],
                    ),
                )
            coverage = tracking["coverage"]
            assert isinstance(coverage, list)
            for interval in coverage:
                assert isinstance(interval, dict)
                connection.execute(
                    "INSERT INTO tracking_coverage(id,started_at,verified_through,"
                    "ended_at,end_reason,configuration_version) VALUES(?,?,?,?,?,?)",
                    (
                        interval["id"],
                        interval["started_at"],
                        interval["verified_through"],
                        interval["ended_at"],
                        interval["end_reason"],
                        interval["configuration_version"],
                    ),
                )
            connection.execute(
                "UPDATE tracking_state SET config=?,configuration_version=?,"
                "revision=revision+1 WHERE singleton=1",
                (json.dumps(tracking["config"]), tracking["configuration_version"]),
            )

    await asyncio.to_thread(install)


async def analytics_report(backend: RustBackend, days: int = 90) -> JsonObjectType:
    """Read the same native endpoint the UI uses."""
    async with (
        ClientSession() as client,
        client.get(
            f"{backend.origin}/v1/tracking/analytics?days={days}",
            headers={"Authorization": f"Bearer {backend.owner_token}"},
        ) as response,
    ):
        assert response.status == 200, await response.text()
        return await response.json()


async def connect_analytics(
    page: Page, backend: RustBackend, query: str = "?closet_view=analytics"
) -> JsonObjectType:
    """Open a bookmark through the same in-memory token form as a real user."""
    await page.goto(backend.origin + "/" + query)
    await page.get_by_label("API token", exact=True).fill(backend.owner_token)
    async with page.expect_response(
        lambda response: "/v1/tracking/analytics?" in response.url
    ) as pending:
        await page.get_by_role("button", name="Connect", exact=True).click()
    response = await pending.value
    assert response.status == 200, await response.text()
    await expect(
        page.get_by_role("heading", name="Analytics", exact=True)
    ).to_be_visible()
    await expect(page.locator("#analytics-state")).not_to_have_text(
        "Loading analytics…"
    )
    return await response.json()


@pytest.mark.parametrize("width", [320, 390, 1100])
async def test_untracked_inventory_has_an_honest_empty_view(
    rust_backend: RustBackend, width: int
) -> None:
    await rust_backend.sync([catalog_item("fictional-untracked")])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            await connect_analytics(page, rust_backend)
            await expect(
                page.get_by_role("heading", name="No wear history yet")
            ).to_be_visible()
            await expect(
                page.get_by_text(
                    "Untracked pieces are not included in Least worn.", exact=False
                )
            ).to_be_visible()
            await expect(
                page.locator(".analytics-summary, .analytics-chart, .analytics-ranked")
            ).to_have_count(0)
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            await page.get_by_role("button", name="Open tracking settings").click()
            await expect(
                page.get_by_role("heading", name="Tracking settings", exact=True)
            ).to_be_visible()
        finally:
            await browser.close()


async def test_view_and_applied_range_bookmark_reload_and_history(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item("fictional-range")])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await connect_analytics(
                page,
                rust_backend,
                "?panel=keep&closet_view=analytics&closet_days=30#catalog",
            )
            await expect(page.get_by_label("Date range", exact=True)).to_have_value(
                "30"
            )
            bookmark = page.url
            await page.get_by_label("Date range", exact=True).select_option("365")
            assert page.url == bookmark
            async with page.expect_response(
                lambda response: "/v1/tracking/analytics?days=365&" in response.url
            ) as pending:
                await page.get_by_role("button", name="Apply range").click()
            assert (await pending.value).status == 200
            assert parse_qs(urlsplit(page.url).query) == {
                "panel": ["keep"],
                "closet_view": ["analytics"],
                "closet_days": ["365"],
            }
            assert urlsplit(page.url).fragment == "catalog"
            bookmark = page.url
            await page.reload()
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Analytics", exact=True)
            ).to_be_visible()
            await expect(page.get_by_label("Date range", exact=True)).to_have_value(
                "365"
            )
            await page.go_back()
            await expect(page.get_by_label("Date range", exact=True)).to_have_value(
                "30"
            )
            await page.go_forward()
            await expect(page.get_by_label("Date range", exact=True)).to_have_value(
                "365"
            )
            assert page.url == bookmark
            assert rust_backend.owner_token not in page.url
            assert not await page.evaluate(
                "localStorage.length + sessionStorage.length"
            )
        finally:
            await browser.close()


async def test_integration_only_connection_does_not_request_admin_analytics(
    rust_backend: RustBackend,
) -> None:
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            requested: list[str] = []
            page.on("request", lambda request: requested.append(request.url))
            await page.goto(rust_backend.origin + "/?closet_view=analytics")
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.integration_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await expect(
                page.get_by_role("button", name="Analytics", exact=True)
            ).to_be_hidden()
            await expect(
                page.get_by_text(
                    "Analytics needs owner or Home Assistant administrator access."
                )
            ).to_be_visible()
            assert not any("/v1/tracking/analytics" in url for url in requested)
        finally:
            await browser.close()


@pytest.mark.parametrize(
    ("width", "theme"), [(320, "light"), (390, "dark"), (1100, "light")]
)
async def test_actual_rankings_photos_chart_detail_and_filter_buttons(
    rust_backend: RustBackend, tmp_path: Path, width: int, theme: str
) -> None:
    await seed_analytics(rust_backend, tmp_path)
    report = await analytics_report(rust_backend)
    assert isinstance(report["inferred_wears"], int) and report["inferred_wears"] > 0
    assert isinstance(report["tracked_units"], int) and isinstance(
        report["total_units"], int
    )
    assert report["tracked_units"] < report["total_units"]
    most = report["most_worn"]
    overlooked = report["overlooked"]
    daily = report["daily"]
    assert isinstance(most, list) and most and isinstance(most[0], dict)
    assert isinstance(overlooked, list) and overlooked and isinstance(daily, list)
    first = most[0]["item"]
    assert isinstance(first, dict) and isinstance(first["name"], str)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900},
                color_scheme="dark" if theme == "dark" else "light",
                is_mobile=width < 600,
                has_touch=width < 600,
            )
            await page.route("https://images.example.invalid/**", fictional_photo)
            await connect_analytics(page, rust_backend)
            await expect(page.locator(".analytics-metric dd").first).to_contain_text(
                str(report["inferred_wears"])
            )
            await expect(
                page.get_by_role(
                    "img", name="Daily inferred wear episodes over 90 days"
                )
            ).to_be_visible()
            if width < 600:
                for viewport in [width, 390, width]:
                    await page.set_viewport_size({"width": viewport, "height": 900})
                    await page.wait_for_function(
                        """() => {
                          const chart = document.querySelector('.analytics-chart');
                          const labels = document.querySelectorAll(
                            '.analytics-chart text'
                          );
                          const frameWidth = chart?.getBoundingClientRect().width;
                          return chart && Math.abs(
                            chart.viewBox.baseVal.width - frameWidth
                          ) < .1 && labels.length > 0 && [...labels].every(
                            label => label.getBoundingClientRect().height >= 10
                          );
                        }""",
                        timeout=5000,
                    )
                    boxes: list[dict[str, float]] = await page.locator(
                        ".analytics-chart text"
                    ).evaluate_all(
                        """nodes => nodes.map(node => {
                          const {width, height} = node.getBoundingClientRect();
                          return {width, height};
                        })"""
                    )
                    assert boxes and all(
                        box["height"] >= 10 and box["width"] > 0 for box in boxes
                    )
                    assert not await page.evaluate(
                        "document.documentElement.scrollWidth > innerWidth"
                    )
            most_region = page.get_by_role("region", name="Most worn", exact=True)
            overlooked_region = page.get_by_role(
                "region", name="Least worn", exact=True
            )
            await expect(most_region.locator(".item-piece")).to_have_count(len(most))
            await expect(most_region.locator(".tracking-pill")).to_have_count(0)
            await expect(
                overlooked_region.locator(":scope > .analytics-ranked .item-piece")
            ).to_have_count(len(overlooked))
            await expect(
                overlooked_region.locator(":scope > .analytics-ranked .tracking-pill")
            ).to_have_text(["Ready to wear"] * len(overlooked))
            for card in await most_region.locator(".item-piece").all():
                image = card.locator(".photo img")
                await image.scroll_into_view_if_needed()
                await expect(image).to_have_js_property("complete", True)
                assert await image.evaluate("image => image.naturalWidth > 0")
                brand = await card.locator(".brand-badge").bounding_box()
                name = await card.locator(".item-name").bounding_box()
                source = await card.locator(".source-badge").bounding_box()
                assert brand and name and source
                assert brand["width"] < name["width"]
                assert brand["y"] + brand["height"] <= name["y"]
                assert name["y"] + name["height"] <= source["y"]
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            await page.get_by_text("View daily data", exact=True).click()
            await expect(
                page.get_by_role(
                    "table", name="Daily wear and reader observation"
                ).locator("tbody tr")
            ).to_have_count(len(daily))
            await most_region.locator(".item-open").first.click()
            await expect(page.get_by_role("dialog")).to_be_visible()
            await expect(page.locator("#detail-title")).to_have_text(first["name"])
            assert parse_qs(urlsplit(page.url).query)["closet_view"] == ["analytics"]
            await page.get_by_role("button", name="Close item details").click()
            await expect(page.get_by_role("dialog")).to_be_hidden()
            if width < 600:
                await most_region.locator(".brand-badge").first.tap()
            else:
                await most_region.locator(".brand-badge").first.click()
            await expect(
                page.get_by_role("heading", name="Inventory", exact=True)
            ).to_be_visible()
            await expect(page.get_by_label("Brand", exact=True)).to_have_value(
                str(first["brand"])
            )
            await expect(page.get_by_label("Brand", exact=True)).not_to_be_focused()
            assert "closet_view" not in parse_qs(urlsplit(page.url).query)
        finally:
            await browser.close()


async def test_another_pick_uses_fresh_server_readiness(
    rust_backend: RustBackend, tmp_path: Path
) -> None:
    await seed_analytics(rust_backend, tmp_path)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://images.example.invalid/**", fictional_photo)
            initial = await connect_analytics(page, rust_backend)
            picked = initial["pick"]
            assert isinstance(picked, dict)
            await command(
                rust_backend,
                {
                    "type": "correct",
                    "unit_id": picked["unit_id"],
                    "status": "needs_laundry",
                    "note": "Fictional wear-readiness correction",
                },
            )
            requested: list[tuple[str, str]] = []
            page.on(
                "request",
                lambda request: requested.append((request.method, request.url)),
            )
            async with page.expect_response(
                lambda response: "/v1/tracking/analytics?" in response.url
            ) as pending:
                await page.get_by_role("button", name="Pick another").click()
            response = await pending.value
            fresh: JsonObjectType = await response.json()
            assert response.status == 200
            ready = fresh["overlooked"]
            assert isinstance(ready, list)
            assert all(
                isinstance(row, dict) and row["unit_id"] != picked["unit_id"]
                for row in ready
            )
            result = fresh["pick"]
            if result is None:
                await expect(
                    page.get_by_role("list", name="Ready overlooked pick")
                ).to_have_count(0)
            else:
                assert (
                    isinstance(result, dict) and result["unit_id"] != picked["unit_id"]
                )
                item = result["item"]
                assert isinstance(item, dict)
                await expect(
                    page.get_by_role("list", name="Ready overlooked pick").locator(
                        ".item-name"
                    )
                ).to_have_text(str(item["name"]))
            queries = [
                parse_qs(urlsplit(url).query)
                for method, url in requested
                if "/v1/tracking/analytics?" in url
            ]
            assert len(queries) == 1 and queries[0]["pick_seed"]
            assert all(method == "GET" for method, _ in requested)
        finally:
            await browser.close()


async def test_late_ranges_and_failed_reads_cannot_leave_mixed_results(
    rust_backend: RustBackend, tmp_path: Path
) -> None:
    await seed_analytics(rust_backend, tmp_path)
    release = asyncio.Event()
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://images.example.invalid/**", fictional_photo)
            await connect_analytics(page, rust_backend)
            held = asyncio.Event()
            fail = False

            async def delayed(route: Route) -> None:
                if fail:
                    await route.fulfill(
                        status=503,
                        content_type="application/json",
                        body='{"error_code":"service_unavailable"}',
                    )
                elif "days=90&" in route.request.url and not held.is_set():
                    response = await route.fetch()
                    held.set()
                    try:
                        await release.wait()
                        await route.fulfill(response=response)
                    finally:
                        await response.dispose()
                else:
                    await route.continue_()

            await page.route("**/v1/tracking/analytics**", delayed)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await asyncio.wait_for(held.wait(), timeout=5)
            await page.get_by_label("Date range", exact=True).select_option("30")
            await page.get_by_role("button", name="Apply range").click()
            chart = page.get_by_role(
                "img", name="Daily inferred wear episodes over 30 days"
            )
            await expect(chart).to_be_visible()
            release.set()
            await expect(chart).to_be_visible()
            await expect(page.get_by_label("Date range", exact=True)).to_have_value(
                "30"
            )
            fail = True
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator("#analytics-state")).to_have_text(
                "Closet or one of its configured services is unavailable."
            )
            await expect(
                page.locator(".analytics-chart, .analytics-ranked")
            ).to_have_count(0)
            await page.unroute("**/v1/tracking/analytics**", delayed)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(chart).to_be_visible()
        finally:
            release.set()
            await browser.close()


async def test_ranked_photos_use_small_previews_while_details_keep_originals(
    rust_backend: RustBackend, tmp_path: Path
) -> None:
    await seed_analytics(rust_backend, tmp_path)
    data: JsonObjectType = json.loads((tmp_path / "analytics-history.json").read_text())
    records = data["catalog"]
    assert isinstance(records, list)
    catalog: list[JsonObjectType] = []
    for index, record in enumerate(records, 1):
        assert isinstance(record, dict)
        record["images"] = [
            {
                "url": f"https://ha.example.invalid/api/image/serve/{index:032x}/original",
                "kind": "retailer_product_reference",
                "confidence": "high",
            }
        ]
        catalog.append(record)
    await rust_backend.sync(catalog, key="fictional-native-photo-previews")
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await page.route("https://ha.example.invalid/**", fictional_photo)
            await connect_analytics(page, rust_backend)
            images = page.locator("#analytics .item-piece .photo img")
            assert await images.count() > 0
            for image in await images.all():
                await expect(image).to_have_attribute("src", re.compile(r"/256x256$"))
                await expect(image).to_have_attribute("loading", "lazy")
                await expect(image).to_have_attribute("decoding", "async")
                await image.scroll_into_view_if_needed()
                await expect(image).to_have_js_property("complete", True)
                assert await image.evaluate("image => image.naturalWidth > 0")
            await (
                page.get_by_role("region", name="Most worn", exact=True)
                .locator(".item-open")
                .first.click()
            )
            await expect(page.locator("#detail .gallery-main img")).to_have_attribute(
                "src", re.compile(r"/original$")
            )
            await page.get_by_role("button", name="Close item details").click()
            await expect(page.get_by_role("dialog")).to_be_hidden()
            await page.get_by_role("button", name="Inventory", exact=True).click()
            await expect(page.locator("#item-grid .item-card")).to_have_count(
                len(catalog)
            )
            for image in await page.locator("#item-grid .photo img").all():
                await expect(image).to_have_attribute("src", re.compile(r"/512x512$"))
        finally:
            await browser.close()
