"""Reader-backed position defaults through real service and browser state."""

from playwright.async_api import async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import ReaderEvidence, command, configure, snapshot
from tests_web.test_app import login
from tests_web.test_tracking import action


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
            add = page.get_by_role("button", name="Add reading position", exact=True)
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
            await new.get_by_label("Position role", exact=True).select_option(
                "enrollment"
            )
            await new.get_by_label("Antenna port", exact=True).fill("4")
            for expected in ("2", "3"):
                await add.click()
                await expect(
                    positions.last.get_by_label("Antenna port", exact=True)
                ).to_have_value(expected)
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
            await page.get_by_role("button", name="Add reader", exact=True).click()
            readers = page.get_by_role("group", name="Reader", exact=True)
            second = readers.nth(1)
            await second.get_by_label("Reader label", exact=True).fill(
                "Example secondary reader"
            )
            await second.get_by_label("Reader enabled", exact=True).uncheck()
            await second.get_by_label("Reader host", exact=True).fill(
                "reader.example.invalid"
            )
            await page.get_by_role(
                "button", name="Add reading position", exact=True
            ).click()
            position = page.get_by_role(
                "group", name="Antenna position", exact=True
            ).last
            choice = position.get_by_label("Reader for this position", exact=True)
            port = position.get_by_label("Antenna port", exact=True)
            await expect(choice).to_have_value("")
            await expect(port).to_have_value("")
            await choice.select_option(label="Example reader")
            await expect(port).to_have_value("2")
            await choice.select_option(label="Example secondary reader")
            await expect(port).to_have_value("")
            await expect(position.locator(".antenna-hint")).to_contain_text(
                "No reader readback"
            )
            await port.fill("3")
            await choice.select_option(label="Example reader")
            await expect(port).to_have_value("2")
            await position.get_by_label("Position role", exact=True).select_option(
                "enrollment"
            )
            await readers.first.get_by_text(
                "Advanced reader settings", exact=True
            ).click()
            original_id = await readers.first.get_by_label(
                "Reader ID", exact=True
            ).input_value()
            conflicting_id = await second.get_by_label(
                "Reader ID", exact=True
            ).input_value()
            identifier = readers.first.get_by_label("Reader ID", exact=True)
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
