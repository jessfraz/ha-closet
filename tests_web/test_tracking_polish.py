"""Reader-backed position defaults through real service and browser state."""

import re

import pytest
from playwright.async_api import Request, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import ReaderEvidence, command, configure, snapshot
from tests_web.test_app import login
from tests_web.test_tracking import action


async def test_lookup_omits_piece_filters_and_does_not_reload_the_catalog(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            await expect(page.get_by_label("Find a piece", exact=True)).to_be_visible()
            await page.get_by_role("button", name="Look up a tag", exact=True).click()
            await expect(
                page.get_by_role("heading", name="Scan a tag to find its garment")
            ).to_be_visible()
            await expect(page.locator("#tag-workspace form.filters")).to_have_count(0)
            requests: list[str] = []

            def catalog_request(request: Request) -> None:
                if "/v1/items?" in request.url:
                    requests.append(request.url)

            page.on("request", catalog_request)
            async with page.expect_response("**/v1/tracking"):
                await page.get_by_role("button", name="Refresh", exact=True).click()
            await page.wait_for_load_state("networkidle")
            await expect(page.locator("#tag-workspace form.filters")).to_have_count(0)
            assert requests == []
            await page.get_by_role("button", name="Enroll", exact=True).click()
            await expect(page.get_by_label("Find a piece", exact=True)).to_be_visible()
        finally:
            await browser.close()


async def test_diagnostics_explain_when_no_reads_have_arrived(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await page.get_by_text(
                "Reader health and evidence diagnostics", exact=True
            ).click()
            await expect(
                page.get_by_role("heading", name="Recent reads")
            ).to_be_visible()
            await expect(
                page.get_by_text("No recent reads.", exact=True)
            ).to_be_visible()
        finally:
            await browser.close()


async def test_reader_connection_has_one_inline_editor_across_areas(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await populated_settings(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await expect(
                page.get_by_label("Area label", exact=True).first
            ).to_be_visible()
            await expect(
                page.get_by_role("button", name="Edit reader connection", exact=True)
            ).to_have_count(0)
            areas = page.get_by_role("group", name="Area", exact=True)
            first = areas.first.get_by_role(
                "group", name="Antenna position", exact=True
            ).first
            other = (
                areas.nth(1)
                .get_by_role("group", name="Antenna position", exact=True)
                .first
            )
            await first.get_by_text("Reader connection", exact=True).click()
            await expect(first.get_by_label("Reader label", exact=True)).to_be_focused()
            await first.get_by_label("Reader label", exact=True).fill(
                "Example renamed shared reader"
            )
            await other.get_by_text("Reader connection", exact=True).click()
            await expect(other.get_by_label("Reader label", exact=True)).to_be_focused()
            await expect(other.get_by_label("Reader label", exact=True)).to_have_value(
                "Example renamed shared reader"
            )
            await expect(page.get_by_label("Reader label", exact=True)).to_have_count(2)
            await expect(
                first.locator(".tracking-reader-settings")
            ).not_to_have_attribute("open", "")
            selected_reader = other.get_by_label("Reader for this position", exact=True)
            await selected_reader.select_option(
                label="Example shared reader b with several antenna connections"
            )
            await expect(other.get_by_label("Reader label", exact=True)).to_have_value(
                "Example shared reader b with several antenna connections"
            )
            await selected_reader.select_option(label="Example renamed shared reader")
            await expect(other.get_by_label("Reader label", exact=True)).to_be_focused()
            await other.get_by_label("Antenna port", exact=True).fill("3")
            await other.get_by_label("Reader label", exact=True).fill(
                "Example shared final reader"
            )
            await action(page, "Save tracking settings")
            saved = (await snapshot(rust_backend))["config"]
            assert isinstance(saved, dict)
            readers = saved["readers"]
            assert isinstance(readers, list) and len(readers) == 2
            assert isinstance(readers[0], dict)
            assert readers[0]["label"] == "Example shared final reader"
            positions = saved["ports"]
            assert isinstance(positions, list) and len(positions) == 6
            assert all(
                isinstance(position, dict)
                and position["reader_id"] == "example-reader-a"
                for position in positions[:4]
            )
        finally:
            await browser.close()


async def test_settings_save_feedback_stays_near_save_and_clears_when_draft_changes(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await populated_settings(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await action(page, "Save tracking settings")
            feedback = page.locator("#tracking-save-feedback")
            await expect(feedback).to_have_text("Tracking settings saved.")
            await expect(feedback).to_have_attribute("role", "status")
            await expect(
                page.get_by_role("status").filter(has_text="Tracking settings saved.")
            ).to_have_count(1)
            save = page.get_by_role("button", name="Save tracking settings", exact=True)
            bounds = await save.bounding_box()
            status = await feedback.bounding_box()
            assert bounds and status
            assert status["y"] >= bounds["y"] + bounds["height"]
            assert status["y"] + status["height"] <= 844
            assert status["y"] - bounds["y"] - bounds["height"] <= 24
            label = page.get_by_label("Area label", exact=True).first
            await label.fill("Example unsaved change")
            await expect(feedback).to_be_hidden()
            await expect(page.locator("#tracking-settings-notice")).to_be_hidden()
            await action(page, "Save tracking settings")
            await expect(feedback).to_have_text("Tracking settings saved.")
            await page.get_by_role("button", name="Add area", exact=True).click()
            await expect(feedback).to_be_hidden()
            await expect(page.locator("#tracking-settings-notice")).to_be_hidden()
        finally:
            await browser.close()


async def test_count_review_identifies_each_acquisition_and_opens_its_record(
    rust_backend: RustBackend,
) -> None:
    records = []
    for ident, size in (
        ("example-tee-xs", "XS"),
        ("example-tee-fitting", "XS"),
        ("example-tee-s", "S"),
        ("example-tee-second-xs", "XS"),
    ):
        item = catalog_item(ident)
        item["size"] = size
        item["quantity"] = 2
        records.append(item)
    records[1]["purchase_source"] = None
    records[1]["shop"] = {
        "id": "example-spring",
        "label": "Spring 2026",
        "season": "Spring",
        "years": [2026],
    }
    await rust_backend.sync(records)
    for item in records:
        item["quantity"] = 1
    await rust_backend.sync(records, key="reduce-fictional-counts")
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 320, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            cards = page.locator("#tracking-settings-content > .tracking-review")
            await expect(cards).to_have_count(4)
            await expect(cards.filter(has_text="Size XS")).to_have_count(3)
            for index, item in enumerate(records):
                ident = str(item["id"])
                card = cards.filter(has=page.get_by_text(ident, exact=True))
                await expect(card).to_have_count(1)
                await expect(card).to_contain_text("Example label Example cotton tee")
                await expect(card).to_contain_text(f"Size {item['size']}")
                await expect(card).to_contain_text(
                    "Spring 2026" if index == 1 else "Self-bought"
                )
                await card.get_by_role(
                    "button", name="Review physical pieces", exact=True
                ).click()
                await expect(page).to_have_url(
                    re.compile(rf"[?&]closet_item={re.escape(ident)}(?:&|$)")
                )
                await expect(page.get_by_role("dialog")).to_be_visible()
                await page.get_by_role("button", name="Close item details").click()
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


async def test_new_positions_use_lowest_unused_reported_ports(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    await ReaderEvidence(rust_backend).observe()
    original = (await snapshot(rust_backend))["config"]
    assert isinstance(original, dict)
    saved_ports = original["ports"]
    assert isinstance(saved_ports, list) and isinstance(saved_ports[0], dict)
    original_id = saved_ports[0]["id"]
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            add = page.get_by_role("button", name="Add pairing antenna", exact=True)
            positions = page.get_by_role("group", name="Antenna position", exact=True)
            await add.click()
            new = positions.last
            await expect(
                new.get_by_label("Reader for this position", exact=True).locator(
                    "option:checked"
                )
            ).to_have_text("Example reader")
            await expect(new.get_by_label("Antenna port", exact=True)).to_have_value(
                "2"
            )
            await new.get_by_text("Advanced position settings", exact=True).click()
            await new.get_by_label("Position role", exact=True).select_option(
                "enrollment"
            )
            await new.get_by_label("Antenna port", exact=True).fill("4")
            for expected in ("2", "3"):
                await add.click()
                await expect(
                    positions.last.get_by_label("Antenna port", exact=True)
                ).to_have_value(expected)
                await positions.last.get_by_text(
                    "Advanced position settings", exact=True
                ).click()
                await positions.last.get_by_label(
                    "Position role", exact=True
                ).select_option("enrollment")
            await add.click()
            await expect(
                positions.last.get_by_label("Antenna port", exact=True)
            ).to_have_value("")
            await expect(positions.last.locator(".antenna-hint")).to_contain_text(
                "All reported antenna ports already have positions"
            )
            await positions.last.get_by_role(
                "button", name="Remove position", exact=True
            ).click()
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            ports = config["ports"]
            assert isinstance(ports, list)
            assert [port["antenna"] for port in ports if isinstance(port, dict)] == [
                1,
                4,
                2,
                3,
            ]
            assert isinstance(ports[0], dict) and ports[0]["id"] == original_id
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()


async def test_reader_change_requires_readback_or_explicit_port_and_keeps_row_identity(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    await ReaderEvidence(rust_backend).observe()
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await page.get_by_role(
                "button", name="Add pairing antenna", exact=True
            ).click()
            positions = page.get_by_role("group", name="Antenna position", exact=True)
            position = positions.last
            choice = position.get_by_label("Reader for this position", exact=True)
            port = position.get_by_label("Antenna port", exact=True)
            await expect(port).to_have_value("2")
            await choice.select_option("new-reader")
            second = position.locator(".tracking-reader-settings")
            await second.get_by_label("Reader label", exact=True).fill(
                "Example secondary reader"
            )
            await second.get_by_label("Reader enabled", exact=True).uncheck()
            await second.get_by_label("Reader host", exact=True).fill(
                "reader.example.invalid"
            )
            conflicting_id = await second.get_by_label(
                "Reader ID", exact=True
            ).input_value()
            await expect(port).to_have_value("")
            await expect(position.locator(".antenna-hint")).to_contain_text(
                "No reader readback"
            )
            await port.fill("3")
            await choice.select_option(label="Example reader")
            await expect(port).to_have_value("2")
            await position.get_by_text("Advanced position settings", exact=True).click()
            await position.get_by_label("Position role", exact=True).select_option(
                "enrollment"
            )
            await positions.first.get_by_text("Reader connection", exact=True).click()
            original = positions.first.locator(".tracking-reader-settings")
            await original.get_by_text("Advanced reader settings", exact=True).click()
            identifier = original.get_by_label("Reader ID", exact=True)
            original_id = await identifier.input_value()
            await identifier.fill(conflicting_id)
            async with page.expect_response("**/v1/tracking/commands") as response:
                await page.get_by_role(
                    "button", name="Save tracking settings", exact=True
                ).click()
            assert (await response.value).status == 400
            await expect(page.locator('[aria-busy="true"]')).to_have_count(0)
            await expect(port).to_have_value("2")
            await identifier.fill(original_id)
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            ports = config["ports"]
            assert isinstance(ports, list) and len(ports) == 2
            assert all(
                isinstance(port, dict) and port["reader_id"] == original_id
                for port in ports
            )
        finally:
            await browser.close()


async def test_custom_reader_endpoint_survives_settings_save(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    config = (await snapshot(rust_backend))["config"]
    assert isinstance(config, dict)
    config["readers"] = [
        {
            "id": "example-custom-reader",
            "label": "Example custom reader",
            "enabled": False,
            "connection": {
                "type": "impinj_llrp",
                "host": "reader.example.invalid",
                "port": 5085,
                "owned_rospec_id": 1,
            },
        }
    ]
    await command(rust_backend, {"type": "configure", "config": config})
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await page.get_by_text("Reader connections", exact=True).click()
            await page.get_by_label("Reader label", exact=True).fill(
                "Example renamed reader"
            )
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            readers = config["readers"]
            assert isinstance(readers, list) and isinstance(readers[0], dict)
            assert readers[0]["connection"] == {
                "type": "impinj_llrp",
                "host": "reader.example.invalid",
                "port": 5085,
                "owned_rospec_id": 1,
            }
            await expect(page.get_by_label("Reader port", exact=True)).to_have_value(
                "5085"
            )
        finally:
            await browser.close()


async def test_areas_start_and_stop_tracking_without_an_extra_switch(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await expect(
                page.get_by_role("button", name="Add area", exact=True)
            ).to_be_visible()
            await expect(
                page.get_by_label("Enable area tracking", exact=True)
            ).to_have_count(0)
            await page.get_by_role("button", name="Add area", exact=True).click()
            await page.get_by_label("Area label", exact=True).fill("Example wardrobe")
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            assert config["enabled"] is True
            areas = config["areas"]
            assert isinstance(areas, list) and len(areas) == 1
            await page.reload()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await expect(page.get_by_label("Area label", exact=True)).to_have_value(
                "Example wardrobe"
            )
            await page.get_by_role("button", name="Remove area", exact=True).click()
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            assert config["areas"] == []
            assert config["enabled"] is False
        finally:
            await browser.close()


@pytest.mark.parametrize("width", [320, 390, 1440])
@pytest.mark.parametrize("color_scheme", ["light", "dark"])
async def test_tracking_sections_have_space_in_empty_and_populated_settings(
    rust_backend: RustBackend, width: int, color_scheme: str
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": width, "height": 900})
            if color_scheme == "dark":
                await page.emulate_media(color_scheme="dark")
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            for populated in (False, True):
                if populated:
                    await page.get_by_role(
                        "button", name="Add area", exact=True
                    ).click()
                    await page.get_by_label("Area label", exact=True).fill(
                        "Example wardrobe"
                    )
                    await page.get_by_role(
                        "button", name="Add antenna", exact=True
                    ).click()
                    await page.get_by_label(
                        "Reader for this position", exact=True
                    ).select_option("new-reader")
                    await page.get_by_label("Reader label", exact=True).fill(
                        "Example reader"
                    )
                    await page.get_by_label("Reader host", exact=True).fill(
                        "reader.example.invalid"
                    )
                sections = page.locator(".tracking-settings-groups > section")
                await expect(sections.first).to_be_visible()
                for index in range(1, await sections.count()):
                    previous = await sections.nth(index - 1).bounding_box()
                    current = await sections.nth(index).bounding_box()
                    assert previous is not None and current is not None
                    assert current["y"] - previous["y"] - previous["height"] >= 24
                bounds = await page.locator("#tracking-settings-content").bounding_box()
                assert bounds is not None and bounds["width"] <= 780
                assert not await page.evaluate(
                    "document.documentElement.scrollWidth > innerWidth"
                )
                if populated:
                    area = page.get_by_role("group", name="Area", exact=True)
                    card_box = await area.bounding_box()
                    add_box = await page.get_by_role(
                        "button", name="Add area", exact=True
                    ).bounding_box()
                    assert card_box is not None and add_box is not None
                    assert add_box["y"] - card_box["y"] - card_box["height"] >= 12
                    await expect(
                        area.get_by_label("Reader label", exact=True)
                    ).to_have_value("Example reader")
        finally:
            await browser.close()


async def test_areas_share_a_reader_and_keep_their_antenna_assignments(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await page.get_by_role("button", name="Add area", exact=True).click()
            areas = page.get_by_role("group", name="Area", exact=True)
            first = areas.first
            await first.get_by_label("Area label", exact=True).fill("Example wardrobe")
            first_id = await first.get_by_label("Area ID", exact=True).input_value()
            await first.get_by_role("button", name="Add antenna", exact=True).click()
            await first.get_by_label(
                "Reader for this position", exact=True
            ).select_option("new-reader")
            await page.get_by_label("Reader label", exact=True).fill(
                "Example shared reader"
            )
            await page.get_by_label("Reader enabled", exact=True).uncheck()
            await page.get_by_label("Reader host", exact=True).fill(
                "reader.example.invalid"
            )
            await first.get_by_label("Antenna port", exact=True).fill("1")
            await page.get_by_role("button", name="Add area", exact=True).click()
            second = areas.nth(1)
            await second.get_by_label("Area label", exact=True).fill("Example hamper")
            await second.get_by_label("Area purpose", exact=True).select_option(
                "hamper"
            )
            second_id = await second.get_by_label("Area ID", exact=True).input_value()
            await second.get_by_role("button", name="Add antenna", exact=True).click()
            await expect(
                second.get_by_label("Reader for this position", exact=True).locator(
                    "option:checked"
                )
            ).to_have_text("Example shared reader")
            await second.get_by_label("Antenna port", exact=True).fill("2")
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            readers, positions = config["readers"], config["ports"]
            assert (
                isinstance(readers, list)
                and len(readers) == 1
                and isinstance(readers[0], dict)
            )
            assert isinstance(positions, list) and len(positions) == 2
            assert {
                (position["area_id"], position["antenna"])
                for position in positions
                if isinstance(position, dict)
            } == {(first_id, 1), (second_id, 2)}
            assert all(
                isinstance(position, dict) and position["reader_id"] == readers[0]["id"]
                for position in positions
            )
            await expect(areas).to_have_count(2)
            await expect(
                areas.first.get_by_label("Antenna port", exact=True)
            ).to_have_value("1")
            await expect(
                areas.nth(1).get_by_label("Antenna port", exact=True)
            ).to_have_value("2")
            await areas.first.get_by_role(
                "button", name="Remove area", exact=True
            ).click()
            await action(page, "Save tracking settings")
            config = (await snapshot(rust_backend))["config"]
            assert isinstance(config, dict)
            positions = config["ports"]
            assert (
                isinstance(positions, list)
                and len(positions) == 1
                and isinstance(positions[0], dict)
            )
            assert positions[0]["area_id"] == second_id and positions[0]["antenna"] == 2
            await expect(
                areas.first.get_by_label("Area label", exact=True)
            ).to_have_value("Example hamper")
        finally:
            await browser.close()


async def populated_settings(backend: RustBackend) -> None:
    config = (await snapshot(backend))["config"]
    assert isinstance(config, dict)
    config.pop("enabled", None)
    config.update(
        areas=[
            {
                "id": "example-closet",
                "label": "Example wardrobe and hanging clothes",
                "kind": "closet",
                "enabled": True,
            },
            {
                "id": "example-hamper",
                "label": "Example laundry hamper",
                "kind": "hamper",
                "enabled": True,
            },
        ],
        readers=[
            {
                "id": f"example-reader-{suffix}",
                "label": (
                    f"Example shared reader {suffix} with several antenna connections"
                ),
                "enabled": False,
                "connection": {
                    "type": "impinj_llrp",
                    "host": f"reader-{suffix}.example.invalid",
                    "port": 5084,
                    "owned_rospec_id": 1,
                },
            }
            for suffix in ("a", "b")
        ],
        ports=[
            {
                "id": f"example-position-{number}",
                "reader_id": "example-reader-a" if number < 5 else "example-reader-b",
                "antenna": number if number < 5 else number - 4,
                "area_id": "example-closet"
                if number < 3
                else "example-hamper"
                if number < 5
                else None,
                "role": "monitor" if number < 5 else "enrollment",
                "enabled": True,
                "required_for_absence": number < 5,
                "commissioned": number < 5,
                "transmit_power_cdbm": None,
                "dwell_ms": None,
            }
            for number in range(1, 7)
        ],
    )
    await command(backend, {"type": "configure", "config": config})


@pytest.mark.parametrize("width", [320, 390])
async def test_populated_settings_controls_fit_with_shared_readers_and_long_labels(
    rust_backend: RustBackend, width: int
) -> None:
    await rust_backend.sync([catalog_item()])
    await populated_settings(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 900}, color_scheme="dark"
            )
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            positions = page.get_by_role("group", name="Antenna position", exact=True)
            await expect(positions).to_have_count(6)
            await positions.first.get_by_text("Reader connection", exact=True).click()
            await positions.first.get_by_text(
                "Advanced reader settings", exact=True
            ).click()
            await positions.first.get_by_text(
                "Advanced position settings", exact=True
            ).click()
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            for control in (
                await page.locator("#tracking-settings").get_by_role("combobox").all()
            ):
                if not await control.is_visible():
                    continue
                bounds = await control.bounding_box()
                assert bounds is not None
                assert bounds["x"] >= 0 and bounds["x"] + bounds["width"] <= width
            await page.get_by_text("Tracking backup and recovery", exact=True).click()
            backup = page.get_by_text(
                "Tracking backup and recovery", exact=True
            ).locator("..")
            text = await backup.locator("p").bounding_box()
            download = await backup.get_by_role(
                "button", name="Download tracking export", exact=True
            ).bounding_box()
            upload = await backup.get_by_label(
                "Restore tracking export", exact=True
            ).bounding_box()
            restore = await backup.get_by_role(
                "button", name="Restore selected export", exact=True
            ).bounding_box()
            assert text and download and upload and restore
            assert download["y"] - text["y"] - text["height"] >= 12
            assert upload["y"] - download["y"] - download["height"] >= 18
            assert restore["y"] - upload["y"] - upload["height"] >= 12
        finally:
            await browser.close()


@pytest.mark.parametrize(
    "changed",
    ["antenna", "reader", "host", "transport", "connection_port", "reader_id", "area"],
)
async def test_hardware_mapping_changes_require_fresh_commissioning(
    rust_backend: RustBackend, changed: str
) -> None:
    await rust_backend.sync([catalog_item()])
    await populated_settings(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            position = page.get_by_role(
                "group", name="Antenna position", exact=True
            ).first
            tested = position.get_by_label("Coverage tested", exact=True)
            await expect(tested).to_be_checked()
            if changed == "antenna":
                await position.get_by_label("Antenna port", exact=True).fill("7")
            elif changed == "reader":
                await position.get_by_label(
                    "Reader for this position", exact=True
                ).select_option(
                    label="Example shared reader b with several antenna connections"
                )
                await position.get_by_label("Antenna port", exact=True).fill("7")
            elif changed == "area":
                await position.get_by_text(
                    "Advanced position settings", exact=True
                ).click()
                await position.get_by_label(
                    "Area for this position", exact=True
                ).select_option(label="Example laundry hamper")
                position = (
                    page.get_by_role("group", name="Area", exact=True)
                    .nth(1)
                    .get_by_role("group", name="Antenna position", exact=True)
                    .last
                )
                tested = position.get_by_label("Coverage tested", exact=True)
            else:
                await position.get_by_text("Reader connection", exact=True).click()
                connection = position.locator(".tracking-reader-settings")
                if changed == "host":
                    await connection.get_by_label("Reader host", exact=True).fill(
                        "replacement-reader.example.invalid"
                    )
                elif changed == "transport":
                    await connection.get_by_label(
                        "Reader transport", exact=True
                    ).select_option("simulator")
                else:
                    await connection.get_by_text(
                        "Advanced reader settings", exact=True
                    ).click()
                    await connection.get_by_label(
                        "Reader port" if changed == "connection_port" else "Reader ID",
                        exact=True,
                    ).fill(
                        "5085"
                        if changed == "connection_port"
                        else "example-replaced-reader"
                    )
            await expect(tested).not_to_be_checked()
            await action(page, "Save tracking settings")
            saved = (await snapshot(rust_backend))["config"]
            assert isinstance(saved, dict) and isinstance(saved["ports"], list)
            selected = next(
                port
                for port in saved["ports"]
                if isinstance(port, dict) and port["id"] == "example-position-1"
            )
            assert isinstance(selected, dict) and selected["commissioned"] is False
            affected = (
                {f"example-position-{number}" for number in range(1, 5)}
                if changed in {"host", "transport", "connection_port", "reader_id"}
                else {"example-position-1"}
            )
            for port in saved["ports"][:4]:
                assert isinstance(port, dict)
                assert port["commissioned"] is (port["id"] not in affected)
            # A deliberate confirmation after the new setup can be saved again.
            target_area = page.get_by_role("group", name="Area", exact=True).nth(
                1 if changed == "area" else 0
            )
            selected_position = target_area.get_by_role(
                "group", name="Antenna position", exact=True
            ).first
            await selected_position.get_by_label("Coverage tested", exact=True).check()
            await action(page, "Save tracking settings")
            saved = (await snapshot(rust_backend))["config"]
            assert isinstance(saved, dict) and isinstance(saved["ports"], list)
            selected = next(
                port
                for port in saved["ports"]
                if isinstance(port, dict) and port["id"] == "example-position-1"
            )
            assert isinstance(selected, dict) and selected["commissioned"] is True
        finally:
            await browser.close()


async def test_label_edits_and_unhealthy_readers_keep_installation_confirmation(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await populated_settings(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            position = page.get_by_role(
                "group", name="Antenna position", exact=True
            ).first
            await expect(
                position.get_by_label("Coverage tested", exact=True)
            ).to_be_checked()
            await page.get_by_label("Area label", exact=True).first.fill(
                "Example renamed wardrobe"
            )
            await position.get_by_text("Reader connection", exact=True).click()
            await position.get_by_label("Reader label", exact=True).fill(
                "Example renamed shared reader"
            )
            await action(page, "Save tracking settings")
            saved = (await snapshot(rust_backend))["config"]
            assert isinstance(saved, dict) and isinstance(saved["ports"], list)
            assert all(
                isinstance(port, dict) and port["commissioned"] is True
                for port in saved["ports"][:4]
            )
            health = (await snapshot(rust_backend))["health"]
            assert (
                isinstance(health, dict) and health["absence_coverage_healthy"] is False
            )
        finally:
            await browser.close()
