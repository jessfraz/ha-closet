"""Photo-led enrollment and server-derived status through the actual UI/API."""

import asyncio
import json
import re
import sqlite3
from pathlib import Path

import pytest
from homeassistant.util.json import JsonObjectType
from playwright.async_api import Page, Route, async_playwright, expect

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.tracking_fixture import (
    ReaderEvidence,
    command,
    configure,
    snapshot,
    units,
)

from .test_app import login
from .test_inventory_ui import product_photo


async def action(page: Page, name: str, *, touch: bool = False) -> None:
    """Await real command acceptance, not just the end of the click gesture."""
    errors: list[str] = []

    def error(error: object) -> None:
        errors.append(str(error))

    page.on("pageerror", error)
    try:
        async with page.expect_response(
            lambda response: (
                response.url.endswith("/v1/tracking/commands")
                and response.request.method == "POST"
            ),
            timeout=5000,
        ) as pending:
            button = page.get_by_role("button", name=name, exact=True)
            if touch:
                await button.tap()
            else:
                await button.click()
        response = await pending.value
        assert response.status == 200, await response.text()
        await expect(page.locator('[aria-busy="true"]')).to_have_count(0)
    finally:
        page.remove_listener("pageerror", error)
        assert not errors, errors


async def test_photo_queue_pair_skip_resume_lookup_and_exceptions(
    rust_backend: RustBackend,
) -> None:
    records = [
        catalog_item(f"example-{index}", name=f"Example garment {index}")
        for index in range(3)
    ]
    records[0]["quantity"] = 2
    await rust_backend.sync(records)
    await configure(rust_backend)
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True
            )
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).tap()
            await expect(
                page.get_by_text("0 tagged · 4 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            assert not await page.get_by_label(
                "Optional confirmation sound"
            ).is_checked()
            await action(page, "Start enrollment session", touch=True)
            await expect(page.locator("#tag-notice")).to_have_text(
                "Enrollment session started."
            )
            current = await snapshot(rust_backend)
            sessions = current["sessions"]
            assert isinstance(sessions, list) and isinstance(sessions[0], dict)
            session_id = sessions[0]["id"]
            assert isinstance(session_id, str)
            await reader.observe(("00000001", "00000002"), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).tap()
            await expect(
                page.get_by_text("More than one tag is present.", exact=False)
            ).to_be_visible()
            confirm = page.get_by_role(
                "button", name="Confirm pairing & next", exact=True
            )
            await expect(confirm).to_be_disabled()
            await action(page, "Scan again", touch=True)
            await reader.observe(("00000001",), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).tap()
            await expect(confirm).to_be_enabled()
            # A stale observation invalidates the visible button without another click.
            await expect(confirm).to_be_disabled(timeout=4000)
            await action(page, "Scan again", touch=True)
            await reader.observe(("00000001",), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).tap()
            await expect(confirm).to_be_enabled()
            await page.get_by_label(
                "Initial area (optional)", exact=True
            ).select_option("example-closet")
            await confirm.tap()
            await expect(
                page.get_by_text("1 tagged · 3 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Tag paired to Example label · Example garment 0"
            )
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Self-bought · piece 1 · Initial area: Example closet"
            )
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "data-tone", "ok"
            )
            await expect(
                page.get_by_label("Initial area (optional)", exact=True)
            ).to_have_value("")
            await expect(confirm).to_be_disabled()
            await page.get_by_role(
                "button", name="Installation queue", exact=True
            ).tap()
            await expect(page.locator("#tag-notice")).to_be_hidden()
            await page.get_by_role("button", name="Look up a tag", exact=True).tap()
            await expect(page.locator("#tag-notice")).to_be_hidden()
            await page.get_by_role("button", name="Enroll", exact=True).tap()
            # All detected tags include a previously-bound ambient tag.
            await reader.observe(("00000001", "00000003"), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).tap()
            await expect(
                page.get_by_text("Detected tag 1 · already paired")
            ).to_be_visible()
            await expect(confirm).to_be_disabled()
            await action(page, "Skip this piece", touch=True)
            after_skip = await snapshot(rust_backend)
            await action(page, "End session")
            await page.reload()
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await page.get_by_role("button", name="Tags", exact=True).click()
            await action(page, "Resume saved queue")
            resumed = after_skip["sessions"]
            assert isinstance(resumed, list) and isinstance(resumed[0], dict)
            queue = resumed[0]["queue"]
            cursor = resumed[0]["cursor"]
            assert isinstance(queue, list) and isinstance(cursor, int)
            expected = next(
                unit for unit in units(after_skip) if unit["id"] == queue[cursor]
            )
            await expect(page.locator(".enroll-copy h2")).to_have_text(
                f"Example garment {str(expected['catalog_id'])[-1]}"
            )
            await action(page, "End session")
            await page.get_by_role("button", name="Look up a tag", exact=True).click()
            await action(page, "Start lookup scan")
            lookups = (await snapshot(rust_backend))["sessions"]
            assert isinstance(lookups, list)
            lookup = next(
                session
                for session in lookups
                if isinstance(session, dict) and session["state"] == "active"
            )
            lookup_id = lookup["id"]
            assert isinstance(lookup_id, str)
            await reader.observe(("00000001",), lookup_id)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(
                page.get_by_text("Detected tag 1 · already paired")
            ).to_be_visible()
            await expect(
                page.get_by_role("button", name="Confirm pairing & next", exact=True)
            ).to_have_count(0)
            await expect(page.locator(".capture-state")).to_have_text(
                "Garment found. Open its details below."
            )
            await page.get_by_role(
                "button", name="View matched Example garment 0", exact=True
            ).click()
            await expect(page.get_by_role("dialog")).to_be_visible()
            await expect(page).to_have_url(re.compile("closet_item=example-0"))
            await page.get_by_role("button", name="Close item details").click()
            await action(page, "End lookup")
            await page.get_by_role(
                "button", name="Installation queue", exact=True
            ).click()
            card = page.locator(".tag-queue-card").nth(1)
            await card.get_by_text("Installation details", exact=True).click()
            await card.get_by_label("Installation", exact=True).select_option("exempt")
            await card.get_by_label("No-tag reason (optional)", exact=True).fill(
                "Sensitive fabric"
            )
            await card.get_by_role(
                "button", name="Save installation", exact=True
            ).click()
            await expect(
                page.get_by_text("1 tagged · 2 remaining · 1 no-tag exceptions")
            ).to_be_visible()
            assert await page.locator("body").evaluate(
                "node => node.scrollWidth <= innerWidth"
            )
        finally:
            await browser.close()


async def test_manual_status_filters_history(
    rust_backend: RustBackend,
) -> None:
    records = [
        catalog_item(f"example-{index:03}", name=f"Example garment {index:03}")
        for index in range(55)
    ]
    await rust_backend.sync(records)
    await configure(rust_backend)
    data = await snapshot(rust_backend)
    target = next(unit for unit in units(data) if unit["catalog_id"] == "example-054")
    await command(
        rust_backend,
        {"type": "baseline", "unit_id": target["id"], "area_id": "example-closet"},
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(timezone_id="America/Los_Angeles")
            await login(page, rust_backend, manual_paging=True)
            await expect(page.get_by_label("Readiness", exact=True)).to_be_visible()
            await expect(page.locator(".item-card .tracking-pill").first).to_have_text(
                "Not tracked"
            )
            await page.get_by_label("Readiness", exact=True).select_option("true")
            await expect(page.locator(".item-card")).to_have_count(1)
            await expect(page.locator(".item-name")).to_have_text("Example garment 054")
            await expect(page.locator(".tracking-pill")).to_have_text("Clean / ready")
            await page.get_by_role(
                "button", name="View Example garment 054", exact=True
            ).click()
            await expect(
                page.get_by_text("Manually confirmed. No RFID certainty is implied.")
            ).to_be_visible()
            await page.get_by_text("Home Assistant entity", exact=True).click()
            await expect(
                page.get_by_text(f"Piece {str(target['id'])[-8:]}", exact=True)
            ).to_be_visible()
            await page.get_by_text("Correct this state", exact=True).click()
            await page.get_by_label("Status", exact=True).last.select_option(
                "needs_laundry"
            )
            await page.get_by_label("Reason", exact=True).fill("Actually in hamper")
            await action(page, "Save correction")
            await expect(page.locator("#detail .tracking-pill")).to_have_text(
                "Needs laundry"
            )
            await page.get_by_text("Movement and corrections", exact=True).click()
            await expect(
                page.get_by_text(
                    "Manual · manual correction: Needs laundry · Actually in hamper",
                    exact=False,
                )
            ).to_be_visible()
            await expect(page.locator(".tracking-history time").first).to_have_text(
                re.compile(r".* UTC$")
            )
            assert "00000001" not in await page.locator("body").inner_text()
            await page.get_by_role(
                "button", name="Close item details", exact=True
            ).click()
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator(".item-card")).to_have_count(0)
        finally:
            await browser.close()


async def test_uncertain_pairing_retries_same_action_and_disconnect_blocks(
    rust_backend: RustBackend,
) -> None:
    record = catalog_item()
    record.update(quantity=2, size="M")
    await rust_backend.sync([record])
    await configure(rust_backend)
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            await action(page, "Start enrollment session")
            sessions = (await snapshot(rust_backend))["sessions"]
            assert isinstance(sessions, list) and isinstance(sessions[0], dict)
            session_id = sessions[0]["id"]
            assert isinstance(session_id, str)
            await reader.observe(("00000009",), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            confirm = page.get_by_role(
                "button", name="Confirm pairing & next", exact=True
            )
            await expect(confirm).to_be_enabled()
            await reader.observe(capture_session=session_id, connected=False)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(confirm).to_be_disabled()
            await expect(page.locator("#service-status")).to_have_text("App connected")
            await expect(page.locator(".reader-recovery")).to_contain_text(
                "power and network connection"
            )
            await expect(page.locator(".reader-recovery")).to_contain_text(
                "Scan again and wait for a fresh isolated tag"
            )
            await page.get_by_role(
                "button", name="Open reader diagnostics", exact=True
            ).click()
            await expect(
                page.locator(".tracking-diagnostics details")
            ).to_have_attribute("open", "")
            await expect(page.locator(".tracking-diagnostics summary")).to_be_focused()
            await expect(
                page.locator(".tracking-diagnostics p.notice")
                .filter(has_text="Example reader · disconnected")
                .first
            ).to_be_visible()
            await page.get_by_role("button", name="Tags", exact=True).click()
            await expect(confirm).to_be_disabled()
            await reader.observe()
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(page.locator(".reader-recovery")).to_have_count(0)
            await action(page, "Scan again")
            await reader.observe(("00000009",), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(confirm).to_be_enabled()
            # Continued reads keep the same candidate selectable. The visible
            # button must validate the newest report, not its original closure.
            for _ in range(2):
                await asyncio.sleep(0.6)
                await reader.observe(("00000009",), session_id)
                await page.get_by_role("button", name="Refresh", exact=True).click()
                await expect(confirm).to_be_enabled()
            requests: list[tuple[str | None, str | None]] = []

            async def lose_first_reply(route: Route) -> None:
                requests.append(
                    (
                        route.request.headers.get("idempotency-key"),
                        route.request.post_data,
                    )
                )
                response = await route.fetch()
                assert response.status == 200
                if len(requests) == 1:
                    await response.dispose()
                    await route.abort("failed")
                else:
                    await route.fulfill(response=response)
                    await response.dispose()

            await page.route("**/v1/tracking/commands", lose_first_reply)
            await confirm.click()
            await expect(
                page.get_by_role("button", name="Retry same action", exact=True)
            ).to_be_visible()
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "data-tone", "warning"
            )
            await expect(page.locator("#tag-notice")).to_have_css(
                "color", "rgb(162, 64, 31)"
            )
            await page.get_by_role("button", name="Look up a tag", exact=True).click()
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "data-tone", "warning"
            )
            await expect(
                page.get_by_role("button", name="Retry same action", exact=True)
            ).to_be_visible()
            await page.get_by_role("button", name="Enroll", exact=True).click()
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "data-tone", "warning"
            )
            await page.get_by_role(
                "button", name="Retry same action", exact=True
            ).click()
            await expect(
                page.get_by_text("1 tagged · 1 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "data-tone", "ok"
            )
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Example cotton tee"
            )
            await expect(page.locator("#tag-notice")).to_contain_text("size M")
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Self-bought · piece 1"
            )
            await expect(page.locator("#tag-notice")).not_to_contain_text("piece 2")
            await expect(page.locator("#tag-notice")).to_have_css(
                "color", "rgb(44, 106, 69)"
            )
            await expect(page.locator("#tag-notice")).to_have_css(
                "background-color", "rgb(231, 242, 234)"
            )
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "role", "status"
            )
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "aria-live", "polite"
            )
            await page.emulate_media(color_scheme="dark")
            await expect(page.locator("#tag-notice")).to_have_css(
                "color", "rgb(145, 210, 168)"
            )
            await expect(page.locator("#tag-notice")).to_have_css(
                "background-color", "rgb(32, 59, 44)"
            )
            assert len(requests) == 2 and requests[0] == requests[1]
            await page.unroute_all(behavior="wait")
            result = await snapshot(rust_backend)
            assert sum(unit["has_binding"] is True for unit in units(result)) == 1
        finally:
            await browser.close()


async def test_reader_settings_save_and_optional_installation_metadata(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 320, "height": 740})
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            await page.get_by_role("button", name="Add area", exact=True).click()
            await page.get_by_label("Area label", exact=True).fill("Fictional rack")
            area_id = await page.get_by_label("Area ID", exact=True).input_value()
            assert area_id.startswith("area-")
            await page.get_by_role("button", name="Add antenna", exact=True).click()
            await page.get_by_label(
                "Reader for this position", exact=True
            ).select_option("new-reader")
            await page.get_by_label("Reader label", exact=True).fill("Fictional reader")
            reader_id = await page.get_by_label("Reader ID", exact=True).input_value()
            assert reader_id.startswith("reader-")
            await page.get_by_label("Reader enabled", exact=True).uncheck()
            await page.get_by_label("Reader host", exact=True).fill(
                "reader.example.invalid"
            )
            await page.get_by_text("Advanced reader settings", exact=True).click()
            await page.get_by_label("Closet-owned ROSpec ID", exact=True).fill("123")
            position_id = await page.get_by_label(
                "Position ID", exact=True
            ).input_value()
            assert position_id.startswith("position-")
            await page.get_by_label(
                "Reader for this position", exact=True
            ).select_option(label="Fictional reader")
            await page.get_by_label("Antenna port", exact=True).fill("1")
            await page.get_by_text("Advanced position settings", exact=True).click()
            await page.get_by_label("Position role", exact=True).select_option(
                "enrollment"
            )
            await expect(
                page.get_by_label("Required for confirmed absence", exact=True)
            ).not_to_be_checked()
            await expect(
                page.get_by_label("Required for confirmed absence", exact=True)
            ).to_be_disabled()
            await page.get_by_label("Position role", exact=True).select_option(
                "monitor"
            )
            await page.get_by_label(
                "Required for confirmed absence", exact=True
            ).check()
            await page.get_by_label("Reader label", exact=True).fill(
                "Fictional renamed reader"
            )
            await expect(
                page.get_by_label("Reader for this position", exact=True).locator(
                    "option:checked"
                )
            ).to_have_text("Fictional renamed reader")
            await action(page, "Save tracking settings")
            await expect(page.get_by_label("Area label", exact=True)).to_have_value(
                "Fictional rack"
            )
            current = await snapshot(rust_backend)
            config = current["config"]
            assert isinstance(config, dict)
            readers = config["readers"]
            assert isinstance(readers, list) and isinstance(readers[0], dict)
            assert readers[0]["connection"] == {
                "type": "impinj_llrp",
                "host": "reader.example.invalid",
                "port": 5084,
                "owned_rospec_id": 123,
            }
            assert readers[0]["id"] == reader_id
            positions = config["ports"]
            assert isinstance(positions, list) and isinstance(positions[0], dict)
            assert positions[0]["id"] == position_id
            assert positions[0]["area_id"] == area_id
            assert positions[0]["reader_id"] == reader_id
            await expect(page.locator("#tracking-save-feedback")).to_have_text(
                "Tracking settings saved."
            )
            await expect(page.locator("#tracking-save-feedback")).to_have_attribute(
                "data-tone", "ok"
            )
            assert await page.locator("body").evaluate(
                "node => node.scrollWidth <= innerWidth"
            )
            await page.get_by_role("button", name="Tags", exact=True).click()
            await page.get_by_role(
                "button", name="Installation queue", exact=True
            ).click()
            await page.get_by_text("Installation details", exact=True).click()
            await page.get_by_label("Installation", exact=True).select_option("queued")
            await action(page, "Save installation")
            await expect(page.get_by_label("Installation", exact=True)).to_have_value(
                "queued"
            )
            changed = units(await snapshot(rust_backend))[0]
            installation = changed["installation"]
            assert isinstance(installation, dict)
            assert installation["state"] == "queued"
            assert (
                installation["tag_model"] is None and installation["placement"] is None
            )
        finally:
            await browser.close()


async def test_enrollment_conflict_restores_start_without_refresh(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        page = await browser.new_page()
        try:
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            start = page.get_by_role(
                "button", name="Start enrollment session", exact=True
            )
            await expect(start).to_be_enabled()

            async def change_revision(route: Route) -> None:
                current = await snapshot(rust_backend)
                await command(
                    rust_backend,
                    {
                        "type": "correct",
                        "unit_id": units(current)[0]["id"],
                        "status": "clean_ready",
                        "note": "Fictional concurrent correction.",
                    },
                )
                await route.continue_()

            await page.route("**/v1/tracking/commands", change_revision)
            async with page.expect_response("**/v1/tracking/commands") as response:
                await start.click()
            assert (await response.value).status == 412
            await expect(page.locator("#tag-notice")).to_be_visible()
            await expect(page.locator('[aria-busy="true"]')).to_have_count(0)
            await expect(start).to_be_enabled()
            await page.unroute_all(behavior="wait")
            await reader.observe()
            await action(page, "Start enrollment session")
            await expect(
                page.get_by_role("button", name="End session", exact=True)
            ).to_be_enabled()
            await action(page, "End session")
        finally:
            await page.unroute_all(behavior="wait")
            await browser.close()


async def test_settings_can_save_twice_without_refresh(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            label = page.get_by_label("Area label", exact=True).first
            save = page.get_by_role("button", name="Save tracking settings", exact=True)
            for value in ("Example rail", "Example updated rail"):
                await label.fill(value)
                await action(page, "Save tracking settings")
                await expect(save).to_be_enabled()
                await expect(label).to_be_enabled()
                await expect(label).to_have_value(value)
                config = (await snapshot(rust_backend))["config"]
                assert isinstance(config, dict)
                areas = config["areas"]
                assert isinstance(areas, list) and isinstance(areas[0], dict)
                assert areas[0]["label"] == value
            # Native form validation accepts the text; the real API rejects a
            # duplicate area ID. Retain the draft and unlock it for correction.
            await page.get_by_text("Advanced area settings", exact=True).first.click()
            area_id = page.get_by_label("Area ID", exact=True).first
            await area_id.fill("example-hamper")
            async with page.expect_response("**/v1/tracking/commands") as response:
                await save.click()
            assert (await response.value).status == 400
            await expect(page.locator('[aria-busy="true"]')).to_have_count(0)
            await expect(page.locator("#tracking-settings-notice")).to_have_attribute(
                "data-tone", "warning"
            )
            await expect(save).to_be_enabled()
            await expect(area_id).to_have_value("example-hamper")
            await area_id.fill("example-closet")
            await action(page, "Save tracking settings")
            await label.fill("Example pending draft")
            await command(
                rust_backend,
                {
                    "type": "correct",
                    "unit_id": units(await snapshot(rust_backend))[0]["id"],
                    "status": "clean_ready",
                    "note": "Fictional concurrent update while editing settings.",
                },
            )
            async with page.expect_response("**/v1/tracking/commands") as response:
                await save.click()
            assert (await response.value).status == 412
            await expect(page.locator('[aria-busy="true"]')).to_have_count(0)
            await expect(save).to_be_enabled()
            await expect(label).to_have_value("Example pending draft")
            # Explicit readback replaces the draft and renews the revision guard.
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(label).to_have_value("Example updated rail")
            await label.fill("Example pending draft")
            await action(page, "Save tracking settings")
        finally:
            await browser.close()


@pytest.mark.parametrize("kind", ["area", "reader"])
async def test_failed_duplicate_id_repair_keeps_each_position_reference(
    rust_backend: RustBackend, kind: str
) -> None:
    await rust_backend.sync([catalog_item()])
    config = (await snapshot(rust_backend))["config"]
    assert isinstance(config, dict)
    config.update(
        areas=[
            {
                "id": "example-closet",
                "label": "Example closet",
                "kind": "closet",
                "enabled": True,
            },
            {
                "id": "example-hamper",
                "label": "Example hamper",
                "kind": "hamper",
                "enabled": True,
            },
        ],
        readers=[
            {
                "id": f"example-reader-{suffix}",
                "label": f"Example reader {suffix}",
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
                "id": f"example-monitor-{suffix}",
                "reader_id": f"example-reader-{suffix}",
                "antenna": 1 if suffix == "a" else 2,
                "area_id": area,
                "role": "monitor",
                "enabled": True,
                "required_for_absence": False,
                "commissioned": False,
                "transmit_power_cdbm": None,
                "dwell_ms": None,
            }
            for suffix, area in (("a", "example-closet"), ("b", "example-hamper"))
        ],
    )
    await command(rust_backend, {"type": "configure", "config": config})
    original = "example-closet" if kind == "area" else "example-reader-a"
    conflicting = "example-hamper" if kind == "area" else "example-reader-b"
    renamed = f"example-renamed-{kind}"
    reference = "area_id" if kind == "area" else "reader_id"
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="Tracking settings", exact=True
            ).click()
            advanced = page.get_by_text(f"Advanced {kind} settings", exact=True).first
            identifier = page.get_by_label(f"{kind.title()} ID", exact=True).first
            if kind == "reader":
                await page.get_by_text("Reader connection", exact=True).first.click()
            await advanced.click()
            await identifier.fill(conflicting)
            async with page.expect_response("**/v1/tracking/commands") as response:
                await page.get_by_role(
                    "button", name="Save tracking settings", exact=True
                ).click()
            assert (await response.value).status == 400
            await expect(page.locator('[aria-busy="true"]')).to_have_count(0)
            await expect(identifier).to_have_value(conflicting)
            await identifier.fill(original)
            await action(page, "Save tracking settings")
            saved = (await snapshot(rust_backend))["config"]
            assert isinstance(saved, dict)
            positions = saved["ports"]
            assert isinstance(positions, list)
            assert [
                (port["id"], port[reference])
                for port in positions
                if isinstance(port, dict)
            ] == [("example-monitor-a", original), ("example-monitor-b", conflicting)]
            # A valid rename still updates only the position linked to that row.
            if kind == "reader":
                await page.get_by_text("Reader connection", exact=True).first.click()
            await advanced.click()
            await identifier.fill(renamed)
            await action(page, "Save tracking settings")
            saved = (await snapshot(rust_backend))["config"]
            assert isinstance(saved, dict)
            positions = saved["ports"]
            assert isinstance(positions, list)
            assert [
                (port["id"], port[reference])
                for port in positions
                if isinstance(port, dict)
            ] == [("example-monitor-a", renamed), ("example-monitor-b", conflicting)]
        finally:
            await browser.close()


@pytest.mark.parametrize("width", [320, 390])
async def test_queue_labels_distinguish_copies_and_mobile_photo_stays_compact(
    rust_backend: RustBackend,
    width: int,
) -> None:
    first = catalog_item("example-copy-a", name="Example shared shirt")
    second = catalog_item("example-copy-b", name="Example shared shirt")
    first["size"], second["size"] = "M", "L"
    for record in (first, second):
        record["brand"] = "Example label with a deliberately lengthy name"
    second["shop"] = {
        "id": "example-atelier",
        "label": "Example atelier",
        "season": None,
        "years": [],
    }
    for record in (first, second):
        record["images"] = [
            {
                "url": "https://images.example.invalid/garment.svg",
                "kind": "retailer_product_reference",
                "confidence": "high",
            }
        ]
    await rust_backend.sync([first, second])
    await configure(rust_backend)
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    physical = units(await snapshot(rust_backend))
    second_unit = next(unit for unit in physical if unit["catalog_id"] == second["id"])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": width, "height": 844}, is_mobile=True, has_touch=True
            )
            await page.route("https://images.example.invalid/**", product_photo)
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).tap()
            chooser = page.get_by_label("Choose a physical piece", exact=True)
            await expect(chooser.locator("option").first).to_contain_text("size M")
            await expect(chooser.locator("option").last).to_contain_text("size L")
            await expect(chooser.locator("option").last).to_contain_text(
                "Example atelier"
            )
            selected_id = second_unit["id"]
            assert isinstance(selected_id, str)
            await chooser.select_option(selected_id)
            await expect(chooser).to_have_value(selected_id)
            summary = page.locator("#selected-copy-details")
            selected_label = await chooser.locator("option:checked").inner_text()
            await expect(summary).to_have_text(selected_label)
            await expect(chooser).to_have_accessible_description(
                "Example label with a deliberately lengthy name · "
                "Example shared shirt · "
                "size L · Cool white · Example atelier · piece 1"
            )
            await expect(summary).to_be_visible()
            assert await summary.evaluate(
                "node => node.scrollWidth <= node.clientWidth"
            )
            await expect(page.locator(".enroll-copy > .muted").first).to_contain_text(
                "size L"
            )
            image = await page.locator(".enroll-photo").bounding_box()
            layout = await page.locator(".enroll-layout").bounding_box()
            start = await page.get_by_role(
                "button", name="Start enrollment session", exact=True
            ).bounding_box()
            assert image and layout and start
            assert image["height"] <= 140
            assert start["y"] - layout["y"] < 400
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
            await action(page, "Start enrollment session", touch=True)
            sessions = (await snapshot(rust_backend))["sessions"]
            assert isinstance(sessions, list) and isinstance(sessions[0], dict)
            queue = sessions[0]["queue"]
            assert isinstance(queue, list) and queue[0] == selected_id
        finally:
            await browser.close()


async def test_unsupported_borrowed_position_requires_dedicated_enrollment(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    config = (await snapshot(rust_backend))["config"]
    assert isinstance(config, dict)
    ports = config["ports"]
    assert isinstance(ports, list) and isinstance(ports[0], dict)
    ports[0].update(role="monitor", area_id="example-closet")
    await command(rust_backend, {"type": "configure", "config": config})
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            position = page.get_by_label("Reading position", exact=True)
            await expect(position).to_have_value("")
            await expect(
                position.locator('option[value="example-enroll"]')
            ).to_have_count(0)
            await expect(
                page.get_by_role("button", name="Start enrollment session", exact=True)
            ).to_be_disabled()
            await expect(
                page.get_by_text(
                    "A dedicated enrollment position is needed.", exact=False
                )
            ).to_be_visible()
            await page.get_by_role(
                "button", name="Open tracking settings", exact=True
            ).click()
            await expect(
                page.get_by_role("heading", name="Tracking settings", exact=True)
            ).to_be_visible()
            await page.get_by_role("button", name="Tags", exact=True).click()
            ports[0].update(role="enrollment", area_id=None)
            await command(rust_backend, {"type": "configure", "config": config})
            await reader.observe()
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(position.locator("option")).to_have_count(1)
            await action(page, "Start enrollment session")
            await expect(
                page.get_by_role("button", name="End session", exact=True)
            ).to_be_enabled()
        finally:
            await browser.close()


async def test_replacement_after_ended_queue_keeps_clicked_piece_in_same_tab(
    rust_backend: RustBackend,
) -> None:
    records = [
        catalog_item("example-a-cardigan", name="Example wool cardigan"),
        catalog_item(
            "example-b-boot", name="Example work boot", category="footwear.boot"
        ),
        catalog_item(
            "example-c-flats", name="Example leather flats", category="footwear.flats"
        ),
    ]
    await rust_backend.sync(records)
    await configure(rust_backend)
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    cardigan = next(
        unit
        for unit in units(await snapshot(rust_backend))
        if unit["catalog_id"] == "example-a-cardigan"
    )
    cardigan_id = cardigan["id"]
    assert isinstance(cardigan_id, str)
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            await page.get_by_label(
                "Choose a physical piece", exact=True
            ).select_option(cardigan_id)
            await action(page, "Start enrollment session")
            sessions = (await snapshot(rust_backend))["sessions"]
            assert isinstance(sessions, list) and isinstance(sessions[0], dict)
            ended_id = sessions[0]["id"]
            assert isinstance(ended_id, str)
            for epc, name in (
                ("00000021", "Example wool cardigan"),
                ("00000022", "Example work boot"),
            ):
                await expect(page.locator(".enroll-copy h2")).to_have_text(name)
                await reader.observe((epc,), ended_id)
                await page.get_by_role("button", name="Refresh", exact=True).click()
                await action(page, "Confirm pairing & next")
            await expect(page.locator(".enroll-copy h2")).to_have_text(
                "Example leather flats"
            )
            await action(page, "End session")
            await page.get_by_role("button", name="Inventory", exact=True).click()
            await page.get_by_role(
                "button", name="View Example wool cardigan", exact=True
            ).click()
            await page.get_by_role("button", name="Replace tag", exact=True).click()
            await expect(page.locator(".enroll-copy h2")).to_have_text(
                "Example wool cardigan"
            )
            await expect(
                page.get_by_label("Choose a physical piece", exact=True)
            ).to_have_value(cardigan_id)
            await expect(
                page.get_by_role("button", name="Scan replacement tag", exact=True)
            ).to_be_enabled()
            await action(page, "Scan replacement tag")
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Replacement scan started for Example label · Example wool cardigan"
            )
            sessions = (await snapshot(rust_backend))["sessions"]
            assert isinstance(sessions, list)
            active = next(
                session
                for session in sessions
                if isinstance(session, dict) and session["state"] == "active"
            )
            assert isinstance(active, dict)
            replacement_id = active["id"]
            assert isinstance(replacement_id, str) and replacement_id != ended_id
            assert active["purpose"] == "replace" and active["queue"] == [cardigan_id]
            assert active["candidates"] == []
            confirm = page.get_by_role("button", name="Confirm replacement", exact=True)
            await expect(confirm).to_be_disabled()
            await reader.observe(("00000023", "00000024"), replacement_id)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(confirm).to_be_disabled()
            await action(page, "Scan again")
            await reader.observe(("00000023",), replacement_id)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await action(page, "Confirm replacement")
            final = units(await snapshot(rust_backend))
            assert {unit["catalog_id"] for unit in final if unit["has_binding"]} == {
                "example-a-cardigan",
                "example-b-boot",
            }
            assert (
                next(
                    unit["id"]
                    for unit in final
                    if unit["catalog_id"] == "example-a-cardigan"
                )
                == cardigan_id
            )
        finally:
            await browser.close()


async def test_replace_unbind_and_undo_keep_physical_piece_identity(
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    reader = ReaderEvidence(rust_backend)
    await reader.observe()
    unit_id = units(await snapshot(rust_backend))[0]["id"]
    assert isinstance(unit_id, str)
    enrolled = await command(
        rust_backend,
        {
            "type": "start_enrollment",
            "unit_ids": [unit_id],
            "port_id": "example-enroll",
            "borrow": False,
            "purpose": "bind",
            "ttl_seconds": 900,
        },
    )
    session = enrolled["session"]
    assert isinstance(session, dict) and isinstance(session["id"], str)
    await reader.observe(("00000011",), session["id"])
    await command(
        rust_backend,
        {
            "type": "select_candidate",
            "session_id": session["id"],
            "capture_version": session["capture_version"],
            "unit_id": unit_id,
            "epc": "00000011",
            "baseline_area": "example-closet",
        },
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role(
                "button", name="View Example cotton tee", exact=True
            ).click()
            await page.get_by_role("button", name="Replace tag", exact=True).click()
            await expect(
                page.get_by_label("Choose a physical piece", exact=True)
            ).to_have_value(unit_id)
            await action(page, "Scan replacement tag")
            sessions = (await snapshot(rust_backend))["sessions"]
            assert isinstance(sessions, list)
            replacement = next(
                entry
                for entry in sessions
                if isinstance(entry, dict) and entry["state"] == "active"
            )
            session_id = replacement["id"]
            assert isinstance(session_id, str)
            await reader.observe(("00000012",), session_id)
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await action(page, "Confirm replacement")
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Replacement tag saved for Example label · Example cotton tee"
            )
            await expect(page.locator("#tag-notice")).to_contain_text(
                "Self-bought · piece 1"
            )
            await expect(page.locator("#tag-notice")).not_to_contain_text(
                "Initial area"
            )
            await expect(page.locator("#tag-notice")).to_have_attribute(
                "data-tone", "ok"
            )
            await expect(
                page.get_by_text("1 tagged · 0 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            after = units(await snapshot(rust_backend))
            assert len(after) == 1 and after[0]["id"] == unit_id
            await page.get_by_role("button", name="Inventory", exact=True).click()
            await page.get_by_role(
                "button", name="View Example cotton tee", exact=True
            ).click()
            neutral_color = await page.get_by_role(
                "button", name="Replace tag", exact=True
            ).evaluate("node => getComputedStyle(node).color")
            unbind = page.get_by_role("button", name="Unbind mistaken tag", exact=True)
            retire = page.get_by_role("button", name="Retire tracking unit", exact=True)
            await expect(unbind).not_to_have_css("color", neutral_color)
            await expect(retire).not_to_have_css("color", neutral_color)
            await unbind.click()
            await expect(
                page.get_by_role("button", name="Confirm unbind", exact=True)
            ).not_to_have_css("color", neutral_color)
            await page.get_by_label("Reason for this change", exact=True).fill(
                "Fictional mistaken pairing"
            )
            async with page.expect_request(
                lambda request: (
                    request.method == "POST"
                    and request.url.endswith("/v1/tracking/commands")
                )
            ) as unbinding:
                await action(page, "Confirm unbind")
            body = (await unbinding.value).post_data
            assert body is not None
            payload: JsonObjectType = json.loads(body)
            assert payload["type"] == "unbind" and payload["mistaken"] is True
            await expect(
                page.get_by_role("button", name="Enroll tag", exact=True)
            ).to_be_visible()
            await page.get_by_role(
                "button", name="Undo last change", exact=True
            ).click()
            await page.get_by_label("Reason for this change", exact=True).fill(
                "Restore the verified pairing"
            )
            await action(page, "Confirm undo")
            await expect(
                page.get_by_role("button", name="Replace tag", exact=True)
            ).to_be_visible()
            restored = units(await snapshot(rust_backend))
            assert (
                len(restored) == 1
                and restored[0]["id"] == unit_id
                and restored[0]["has_binding"] is True
            )
            await retire.click()
            await expect(
                page.get_by_role("button", name="Confirm retirement", exact=True)
            ).not_to_have_css("color", neutral_color)
        finally:
            await browser.close()


async def test_quantity_reconciliation_restores_units_without_overriding_source(
    rust_backend: RustBackend,
) -> None:
    record = catalog_item()
    record["quantity"] = 2
    await rust_backend.sync([record])
    current = units(await snapshot(rust_backend))
    await command(
        rust_backend,
        {
            "type": "retire",
            "unit_id": current[0]["id"],
            "note": "Fictional mistaken retirement",
        },
    )
    await command(
        rust_backend,
        {
            "type": "installation",
            "unit_id": current[1]["id"],
            "installation": {
                "state": "exempt",
                "no_tag_reason": "Fictional comfort exception",
            },
        },
    )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            await expect(page.locator("#tag-progress")).to_contain_text(
                "1 unallocated piece"
            )
            await expect(
                page.get_by_role("heading", name="More pieces need review", exact=True)
            ).to_be_visible()
            await expect(
                page.get_by_label("Choose a physical piece", exact=True)
            ).to_have_count(0)
            await expect(
                page.get_by_role("heading", name="The queue is complete", exact=True)
            ).to_have_count(0)
            warning_button = page.get_by_role(
                "button", name="Review tracking setup", exact=True
            )
            warning = warning_button.locator("..")
            copy = await warning.locator("p").bounding_box()
            control = await warning_button.bounding_box()
            assert copy and control and control["y"] - copy["y"] - copy["height"] >= 12
            await warning_button.click()
            await expect(
                page.get_by_role("button", name="Confirm physical count", exact=True)
            ).to_have_count(0)
            await action(page, "Restore missing physical units")
            await expect(
                page.get_by_role(
                    "button", name="Restore missing physical units", exact=True
                )
            ).to_have_count(0)
            restored = units(await snapshot(rust_backend))
            assert sum(unit["retired"] is False for unit in restored) == 2
            assert any(unit["id"] == current[1]["id"] for unit in restored)
            record["quantity"] = 1
            await rust_backend.sync([record], "reduce-source-quantity")
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(
                page.get_by_text(
                    "The source quantity cannot be overridden here.", exact=False
                )
            ).to_be_visible()
            await expect(
                page.get_by_role("button", name="Confirm physical count", exact=True)
            ).to_have_count(0)
            await expect(
                page.get_by_role(
                    "button", name="Restore missing physical units", exact=True
                )
            ).to_have_count(0)
            await page.get_by_role(
                "button", name="Review physical pieces", exact=True
            ).click()
            await expect(
                page.get_by_role("button", name="Retire tracking unit", exact=True)
            ).to_have_count(2)
        finally:
            await browser.close()


async def test_legacy_unknown_quantity_cannot_claim_enrollment_complete(
    rust_backend: RustBackend,
    tmp_path: Path,
) -> None:
    """Historical nullable metadata remains usable, without fabricated pieces."""
    record = catalog_item("legacy-example", name="Historical example garment")
    await rust_backend.sync([record])

    def historical_storage() -> str:
        # Only this disposable database is seeded into the supported historical
        # nullable shape. New imports still reject missing quantities.
        with sqlite3.connect(tmp_path / "catalog.sqlite3") as database:
            database.execute("PRAGMA foreign_keys=ON")
            database.execute("BEGIN IMMEDIATE")
            saved = database.execute(
                "SELECT body FROM items WHERE id=?", (record["id"],)
            ).fetchone()
            assert saved is not None and isinstance(saved[0], str)
            body: JsonObjectType = json.loads(saved[0])
            body["quantity"] = None
            serialized = json.dumps(body)
            database.execute(
                "DELETE FROM tracking_history WHERE unit_id IN "
                "(SELECT id FROM tracking_units WHERE catalog_id=?)",
                (record["id"],),
            )
            database.execute(
                "DELETE FROM tracking_units WHERE catalog_id=?", (record["id"],)
            )
            database.execute(
                "UPDATE items SET body=? WHERE id=?", (serialized, record["id"])
            )
            database.execute(
                "UPDATE tracking_inventory SET catalog_quantity=NULL,"
                "declared_quantity=NULL,next_ordinal=1,needs_reconciliation=1 "
                "WHERE catalog_id=?",
                (record["id"],),
            )
            database.execute(
                "UPDATE tracking_state SET revision=revision+1 WHERE singleton=1"
            )
        return serialized

    original = await asyncio.to_thread(historical_storage)
    before = await snapshot(rust_backend)
    assert units(before) == []
    counts = before["counts"]
    assert isinstance(counts, dict) and counts["quantity_unknown_items"] == 1
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page()
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            await expect(
                page.get_by_role("heading", name="The queue is complete", exact=True)
            ).to_have_count(0)
            await expect(
                page.get_by_role("heading", name="More pieces need review", exact=True)
            ).to_be_visible()
            await expect(page.locator("#tag-progress")).to_contain_text(
                "1 quantity needs review"
            )
            await page.get_by_role(
                "button", name="Review tracking setup", exact=True
            ).click()
            await page.get_by_label("Confirmed physical count", exact=True).fill("2")
            await action(page, "Confirm physical count")
            await page.get_by_role("button", name="Tags", exact=True).click()
            await expect(page.locator("#tag-progress")).to_have_text(
                "0 tagged · 2 remaining · 0 no-tag exceptions"
            )
            await expect(
                page.get_by_role("button", name="Review tracking setup", exact=True)
            ).to_have_count(0)
            for unit in units(await snapshot(rust_backend)):
                await command(
                    rust_backend,
                    {
                        "type": "installation",
                        "unit_id": unit["id"],
                        "installation": {
                            "state": "exempt",
                            "no_tag_reason": "Fictional comfort exception",
                        },
                    },
                )
            await page.get_by_role("button", name="Refresh", exact=True).click()
            await expect(
                page.get_by_role("heading", name="The queue is complete", exact=True)
            ).to_be_visible()
            await expect(page.locator("#tag-progress")).to_have_text(
                "0 tagged · 0 remaining · 2 no-tag exceptions"
            )
        finally:
            await browser.close()

    def read_source() -> str:
        with sqlite3.connect(tmp_path / "catalog.sqlite3") as database:
            saved = database.execute(
                "SELECT body FROM items WHERE id=?", (record["id"],)
            ).fetchone()
            assert saved is not None and isinstance(saved[0], str)
            return saved[0]

    assert await asyncio.to_thread(read_source) == original


async def test_manual_status_filters_remain_complete_after_bookmark_reload(
    rust_backend: RustBackend,
) -> None:
    records = [
        catalog_item("example-manual-wash"),
        catalog_item("example-manual-laundry"),
    ]
    await rust_backend.sync(records)
    for unit in units(await snapshot(rust_backend)):
        await command(
            rust_backend,
            {
                "type": "correct",
                "unit_id": unit["id"],
                "status": "in_wash"
                if unit["catalog_id"] == "example-manual-wash"
                else "needs_laundry",
                "note": "Fictional manual status without a reader",
            },
        )
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(viewport={"width": 390, "height": 844})
            await login(page, rust_backend)
            status = page.get_by_label("Status", exact=True)
            await expect(status).to_be_visible()
            await expect(status.locator("option")).to_have_count(9)
            await status.select_option("in_wash")
            await expect(
                page.locator('[data-catalog-id="example-manual-wash"]')
            ).to_be_visible()
            bookmarked = page.url
            await page.reload()
            await page.get_by_label("API token", exact=True).fill(
                rust_backend.owner_token
            )
            await page.get_by_role("button", name="Connect", exact=True).click()
            await expect(page).to_have_url(bookmarked)
            await expect(status).to_have_value("in_wash")
            await expect(status.locator("option")).to_have_count(9)
            await status.select_option("needs_laundry")
            await expect(
                page.locator('[data-catalog-id="example-manual-laundry"]')
            ).to_be_visible()
            await expect(
                page.locator('[data-catalog-id="example-manual-wash"]')
            ).to_have_count(0)
        finally:
            await browser.close()


@pytest.mark.parametrize("mode", ["Enroll", "Look up a tag"])
async def test_scan_controls_leave_space_for_guidance_on_mobile(
    rust_backend: RustBackend, mode: str
) -> None:
    await rust_backend.sync([catalog_item()])
    async with async_playwright() as playwright:
        browser = await playwright.chromium.launch()
        try:
            page = await browser.new_page(
                viewport={"width": 320, "height": 844}, color_scheme="dark"
            )
            await login(page, rust_backend)
            await page.get_by_role("button", name="Tags", exact=True).click()
            await page.get_by_role("button", name=mode, exact=True).click()
            station = page.locator(".station-controls")
            await expect(station).to_be_visible()
            previous = await station.locator(
                "xpath=preceding-sibling::p[1]"
            ).bounding_box()
            controls = await station.bounding_box()
            assert (
                previous
                and controls
                and controls["y"] - previous["y"] - previous["height"] >= 16
            )
            actions = await station.locator(".enroll-actions").bounding_box()
            helper = await station.get_by_text(
                "A dedicated enrollment position is needed.", exact=False
            ).bounding_box()
            assert (
                actions
                and helper
                and helper["y"] - actions["y"] - actions["height"] >= 12
            )
            if mode == "Enroll":
                photo = await page.locator(".enroll-photo").bounding_box()
                assert photo and controls["y"] >= photo["y"] + photo["height"]
            assert not await page.evaluate(
                "document.documentElement.scrollWidth > innerWidth"
            )
        finally:
            await browser.close()
