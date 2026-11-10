"""Real Rust tracking, authenticated HA boundary and entity lifetimes."""

import json
from uuid import uuid4

from homeassistant.core import HomeAssistant, State
from homeassistant.helpers import entity_registry as er
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.closet.const import DOMAIN
from custom_components.closet.diagnostics import async_get_config_entry_diagnostics

from .conftest import RustBackend, catalog_item
from .test_integration import PREFIX, connect
from .tracking_fixture import command, configure, snapshot, units


def state(hass: HomeAssistant, entity_id: str) -> State:
    """A missing entity is a test failure, not an unavailable measurement."""
    result = hass.states.get(entity_id)
    assert result is not None
    return result


async def test_tracking_health_follows_areas_without_a_global_switch(
    hass: HomeAssistant,
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    entry = await connect(hass, rust_backend)
    registry = er.async_get(hass)
    health_id = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_tracking_health"
    )
    assert health_id is not None
    assert state(hass, health_id).state == "disabled"
    current = await snapshot(rust_backend)
    original_units = units(current)
    config = current["config"]
    assert isinstance(config, dict)
    # An older client may still send the old switch. Areas determine activation.
    config["enabled"] = False
    config["areas"] = [
        {
            "id": "fictional-rack",
            "label": "Fictional rack",
            "kind": "rewear",
            "enabled": True,
        }
    ]
    await command(rust_backend, {"type": "configure", "config": config})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    active = await snapshot(rust_backend)
    active_config = active["config"]
    assert isinstance(active_config, dict) and active_config["enabled"] is True
    assert state(hass, health_id).state in {"warming_up", "needs_attention"}
    assert units(active) == original_units

    config["enabled"] = True
    config["areas"] = []
    await command(rust_backend, {"type": "configure", "config": config})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    stopped = await snapshot(rust_backend)
    stopped_config = stopped["config"]
    assert isinstance(stopped_config, dict) and stopped_config["enabled"] is False
    assert state(hass, health_id).state == "disabled"
    assert units(stopped) == original_units


async def test_tracking_manual_readiness_entities_and_restore(
    hass: HomeAssistant,
    rust_backend: RustBackend,
) -> None:
    records = [
        catalog_item("fictional-one"),
        catalog_item("fictional-two"),
        catalog_item("fictional-three"),
    ]
    await rust_backend.sync(records)
    await configure(rust_backend)
    unit = units(await snapshot(rust_backend))[0]
    unit_id = unit["id"]
    entry = await connect(hass, rust_backend)
    registry = er.async_get(hass)
    ready_id = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_tracking_certified_ready"
    )
    health_id = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_tracking_health"
    )
    piece_id = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_unit_{unit_id}"
    )
    assert ready_id is not None and health_id is not None and piece_id is not None
    assert state(hass, ready_id).state == "0"
    assert state(hass, health_id).state != "unavailable"
    piece = registry.async_get(piece_id)
    assert (
        piece is not None and piece.disabled_by is er.RegistryEntryDisabler.INTEGRATION
    )
    registry.async_update_entity(piece_id, disabled_by=None, name="Custom piece status")
    assert await hass.config_entries.async_reload(entry.entry_id)
    await hass.async_block_till_done()
    assert state(hass, piece_id).state == "unknown"
    await command(
        rust_backend,
        {"type": "baseline", "unit_id": unit_id, "area_id": "example-closet"},
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert state(hass, ready_id).state == "1"
    piece_state = state(hass, piece_id)
    assert piece_state.state == "clean_ready"
    assert piece_state.attributes["evidence"] == "manual"
    assert piece_state.attributes["ready_to_wear"] is True
    assert "epc" not in json.dumps(piece_state.attributes).lower()
    await command(rust_backend, {"type": "pause", "unit_id": unit_id, "paused": True})
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert state(hass, ready_id).state == "1"
    assert state(hass, piece_id).attributes["automation_paused"] is True
    await command(
        rust_backend,
        {
            "type": "correct",
            "unit_id": unit_id,
            "status": "needs_laundry",
            "note": "Fictional manual correction",
        },
    )
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert state(hass, ready_id).state == "0"
    assert state(hass, ready_id).state != "unavailable"
    retained = [record for record in records if record["id"] != unit["catalog_id"]][:1]
    await rust_backend.sync(retained, "archive-fixture")
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert state(hass, piece_id).state == "unavailable"
    await rust_backend.sync(records, "restore-fixture")
    await entry.runtime_data.async_refresh()
    await hass.async_block_till_done()
    assert (
        registry.async_get_entity_id(
            "sensor", DOMAIN, f"{entry.entry_id}_unit_{unit_id}"
        )
        == piece_id
    )
    restored = registry.async_get(piece_id)
    assert restored is not None and restored.name == "Custom piece status"
    assert state(hass, piece_id).state != "unavailable"
    diagnostics = await async_get_config_entry_diagnostics(hass, entry)
    text = json.dumps(diagnostics)
    for private in [
        str(unit_id),
        "fictional-one",
        "example-reader",
        "127.0.0.1",
        "epc",
        "rssi",
    ]:
        assert private not in text.lower()
    assert await hass.config_entries.async_unload(entry.entry_id)
    await hass.async_block_till_done()
    assert state(hass, ready_id).state == "unavailable"


async def test_tracking_proxy_auth_replay_and_fixed_routes(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    await connect(hass, rust_backend)
    reader = await hass_client(hass_user_token)
    projection_response = await reader.get(f"{PREFIX}/tracking/projection?limit=1")
    assert projection_response.status == 200
    projection = await projection_response.json()
    assert len(projection["units"]) == 1
    assert set(projection["units"][0]) == {
        "id",
        "ordinal",
        "status",
        "paused",
        "retired",
        "archived",
        "evidence",
    }
    current = await (await reader.get(f"{PREFIX}/tracking")).json()
    assert current["sessions"] == []
    unit = current["units"][0]
    headers = {
        "If-Match": f'"{current["tracking_revision"]}"',
        "Idempotency-Key": str(uuid4()),
        "X-Closet-Actor-Admin": "true",
    }
    body = {"type": "baseline", "unit_id": unit["id"], "area_id": "example-closet"}
    assert (
        await reader.post(f"{PREFIX}/tracking/commands", json=body, headers=headers)
    ).status == 403
    assert (await reader.post(f"{PREFIX}/tracking/observations", json={})).status == 403
    assert (await reader.get(f"{PREFIX}/tracking/export")).status == 403
    assert (await reader.get(f"{PREFIX}/tracking/coverage")).status == 403
    assert (await reader.get(f"{PREFIX}/tracking/analytics")).status == 403
    assert (
        await reader.get(f"{PREFIX}/tracking/units/{unit['id']}/history")
    ).status == 403
    admin = await hass_client()
    response = await admin.post(
        f"{PREFIX}/tracking/commands", json=body, headers=headers
    )
    assert response.status == 200, await response.text()
    applied = await response.json()
    replay = await admin.post(f"{PREFIX}/tracking/commands", json=body, headers=headers)
    assert replay.status == 200 and await replay.json() == applied
    history = await admin.get(f"{PREFIX}/tracking/units/{unit['id']}/history?limit=1")
    assert history.status == 200
    assert len((await history.json())["events"]) == 1
    coverage = await admin.get(f"{PREFIX}/tracking/coverage?limit=1")
    assert coverage.status == 200
    assert (await coverage.json())["intervals"] == []
    analytics = await admin.get(f"{PREFIX}/tracking/analytics?days=30&pick_seed=8")
    assert analytics.status == 200
    report = await analytics.json()
    assert report["days"] == 30 and report["inferred_wears"] == 0
    assert report["overlooked"] == [] and report["pick"] is None
    assert (await admin.get(f"{PREFIX}/tracking/analytics?days=31")).status == 400
    assert (await admin.get(f"{PREFIX}/tracking/coverage?epc=private")).status == 400
    assert (await admin.get(f"{PREFIX}/tracking?epc=private")).status == 400
    assert (
        await admin.get(f"{PREFIX}/tracking/units/not-a-uuid/history")
    ).status == 404
    assert (
        await admin.get(
            f"{PREFIX}/tracking/units/{unit['id']}/history?url=http://example.invalid"
        )
    ).status == 400
    assert (await admin.post(f"{PREFIX}/tracking/write-tag", json={})).status == 404
    assert (
        await admin.get(
            f"{PREFIX}/items?ready_to_wear=true&status=clean_ready&area=example-closet"
        )
    ).status == 200


async def test_tracking_import_bridge_accepts_real_export_above_inventory_limit(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    """The operational export envelope has its own bounded eight-MiB contract."""
    from aiohttp import ClientSession

    records = [catalog_item(f"bulk-example-{index}") for index in range(4)]
    for record in records:
        record["quantity"] = 1000
    await rust_backend.sync(records)
    async with ClientSession() as upstream:
        response = await upstream.get(
            f"{rust_backend.origin}/v1/tracking/export",
            headers={"Authorization": f"Bearer {rust_backend.owner_token}"},
        )
        assert response.status == 200
        exported = await response.json()
    payload = json.dumps({"type": "import", "data": exported}).encode()
    assert 2 * 1024 * 1024 < len(payload) < 8 * 1024 * 1024
    entry = await connect(hass, rust_backend)
    before = await snapshot(rust_backend)
    client = await hass_client()
    response = await client.post(
        f"{PREFIX}/tracking/commands",
        data=payload,
        headers={
            "Content-Type": "application/json",
            "If-Match": f'"{before["tracking_revision"]}"',
            "Idempotency-Key": "large-real-tracking-import",
        },
    )
    assert response.status == 200, await response.text()
    await hass.async_block_till_done()
    await entry.runtime_data.async_refresh()
    assert entry.runtime_data.data.item_count == 4
    tracking = entry.runtime_data.data.tracking
    assert tracking is not None and len(tracking.units) == 4000
    for path, limit in [
        ("inventory/validate", 2 * 1024 * 1024),
        ("tracking/observations", 1024 * 1024),
        ("tracking/commands", 8 * 1024 * 1024),
    ]:
        oversized = await client.post(
            f"{PREFIX}/{path}",
            data=b" " * (limit + 1),
            headers={"Content-Type": "application/json"},
        )
        assert oversized.status == 413


async def test_maximum_active_projection_fits_real_ha_client_and_coordinator(
    hass: HomeAssistant,
    rust_backend: RustBackend,
) -> None:
    """A full supported population must not require raising HA's byte limit."""
    import pytest
    from homeassistant.helpers.aiohttp_client import async_get_clientsession
    from pytest_homeassistant_custom_component.common import MockConfigEntry

    from custom_components.closet.client import ClosetClient, ResponseTooLarge
    from custom_components.closet.const import MAX_RESPONSE_BYTES
    from custom_components.closet.coordinator import ClosetCoordinator

    records = [catalog_item(f"capacity-example-{index}") for index in range(20)]
    for record in records:
        record["quantity"] = 1000
    await rust_backend.sync(records)
    client = ClosetClient(
        async_get_clientsession(hass),
        rust_backend.origin,
        rust_backend.integration_token,
    )
    projection = await client.request("GET", "/v1/tracking/projection")
    assert projection.status == 200
    assert len(projection.body) < MAX_RESPONSE_BYTES
    # The enrollment document is intentionally not the HA polling contract.
    with pytest.raises(ResponseTooLarge):
        await client.request("GET", "/v1/tracking")
    entry = MockConfigEntry(domain=DOMAIN)
    entry.add_to_hass(hass)
    coordinator = ClosetCoordinator(hass, entry, client)
    try:
        await coordinator.async_refresh()
        assert coordinator.last_update_success
        tracking = coordinator.data.tracking
        assert tracking is not None
        assert len(tracking.units) == 20_000
        assert tracking.counts["total_units"] == 20_000
        assert tracking.counts["certified_ready"] == 0
        assert all(
            unit.status == "unknown" and not unit.ready for unit in tracking.units
        )
    finally:
        await coordinator.async_shutdown()
