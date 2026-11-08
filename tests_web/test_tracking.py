"""Photo-led enrollment and server-derived status through the actual UI/API."""

import asyncio
import json
import sqlite3
from pathlib import Path

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
            await confirm.tap()
            await expect(
                page.get_by_text("1 tagged · 3 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            await expect(confirm).to_be_disabled()
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
            page = await browser.new_page()
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
    await rust_backend.sync([catalog_item()])
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
            await reader.observe()
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
            await page.get_by_role(
                "button", name="Retry same action", exact=True
            ).click()
            await expect(
                page.get_by_text("1 tagged · 0 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            assert len(requests) == 2 and requests[0] == requests[1]
            await page.unroute("**/v1/tracking/commands", lose_first_reply)
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
            await page.get_by_label("Enable area tracking", exact=True).check()
            await page.get_by_role("button", name="Add area", exact=True).click()
            await page.get_by_label("Area ID", exact=True).fill("fictional-rack")
            await page.get_by_label("Area label", exact=True).fill("Fictional rack")
            await page.get_by_role("button", name="Add reader", exact=True).click()
            await page.get_by_label("Reader ID", exact=True).fill("fictional-reader")
            await page.get_by_label("Reader label", exact=True).fill("Fictional reader")
            await page.get_by_label("Reader enabled", exact=True).uncheck()
            await page.get_by_label("Reader host", exact=True).fill(
                "reader.example.invalid"
            )
            await page.get_by_label("Closet-owned ROSpec ID", exact=True).fill("123")
            await page.get_by_role(
                "button", name="Add reading position", exact=True
            ).click()
            await page.get_by_label("Position ID", exact=True).fill(
                "fictional-position"
            )
            await page.get_by_label("Reader ID for this position", exact=True).fill(
                "fictional-reader"
            )
            await page.get_by_label("Area ID (blank for enrollment)", exact=True).fill(
                "fictional-rack"
            )
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
            assert await page.locator("body").evaluate(
                "node => node.scrollWidth <= innerWidth"
            )
            await page.get_by_role("button", name="Tags", exact=True).click()
            await page.get_by_role(
                "button", name="Installation queue", exact=True
            ).click()
            await page.get_by_text("Installation details", exact=True).click()
            await page.get_by_label("Installation", exact=True).select_option(
                "seamstress_queued"
            )
            await action(page, "Save installation")
            await expect(page.get_by_label("Installation", exact=True)).to_have_value(
                "seamstress_queued"
            )
            changed = units(await snapshot(rust_backend))[0]
            installation = changed["installation"]
            assert isinstance(installation, dict)
            assert installation["state"] == "seamstress_queued"
            assert (
                installation["tag_model"] is None and installation["placement"] is None
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
            await expect(
                page.get_by_text("1 tagged · 0 remaining · 0 no-tag exceptions")
            ).to_be_visible()
            after = units(await snapshot(rust_backend))
            assert len(after) == 1 and after[0]["id"] == unit_id
            await page.get_by_role("button", name="Inventory", exact=True).click()
            await page.get_by_role(
                "button", name="View Example cotton tee", exact=True
            ).click()
            await page.get_by_role(
                "button", name="Unbind mistaken tag", exact=True
            ).click()
            await page.get_by_label("Reason for this change", exact=True).fill(
                "Fictional mistaken pairing"
            )
            await action(page, "Confirm unbind")
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
                page.get_by_role("heading", name="The queue is complete", exact=True)
            ).to_have_count(0)
            await page.get_by_role(
                "button", name="Review tracking setup", exact=True
            ).click()
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
