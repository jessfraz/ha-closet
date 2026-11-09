"""The real read-only IoT adapter cannot silently borrow a monitoring antenna."""

import asyncio

from homeassistant.core import HomeAssistant
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from tests_ha.conftest import RustBackend, catalog_item
from tests_ha.test_integration import PREFIX, connect
from tests_ha.tracking_fixture import command, configure, snapshot, units


async def test_iot_borrow_reports_unsupported_but_dedicated_enrollment_succeeds(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    await rust_backend.sync([catalog_item()])
    await configure(rust_backend)
    current = await snapshot(rust_backend)
    config = current["config"]
    assert isinstance(config, dict)
    ports = config["ports"]
    assert isinstance(ports, list)
    ports.append(
        {
            "id": "example-monitor",
            "reader_id": "example-reader",
            "antenna": 2,
            "area_id": "example-closet",
            "role": "monitor",
            "enabled": True,
            "required_for_absence": True,
            "commissioned": False,
        }
    )
    await command(rust_backend, {"type": "configure", "config": config})
    async with asyncio.timeout(5):
        while True:
            current = await snapshot(rust_backend)
            readbacks = current["reader_readbacks"]
            assert isinstance(readbacks, list)
            if readbacks:
                break
            await asyncio.sleep(0.01)
    readback = readbacks[0]
    assert isinstance(readback, dict)
    capability = readback["capabilities"]
    assert isinstance(capability, dict) and capability["inventory_pause"] is False
    await connect(hass, rust_backend)
    client = await hass_client()
    unit_id = units(current)[0]["id"]
    before = await snapshot(rust_backend)
    response = await client.post(
        f"{PREFIX}/tracking/commands",
        json={
            "type": "start_enrollment",
            "unit_ids": [unit_id],
            "port_id": "example-monitor",
            "borrow": True,
            "purpose": "bind",
            "ttl_seconds": 120,
        },
        headers={
            "If-Match": f'"{before["tracking_revision"]}"',
            "Idempotency-Key": "unsupported-borrow",
        },
    )
    assert response.status == 409
    assert (await response.json())["error_code"] == "borrow_unsupported"
    after = await snapshot(rust_backend)
    assert after["tracking_revision"] == before["tracking_revision"]
    assert after["sessions"] == []
    await command(
        rust_backend,
        {
            "type": "start_enrollment",
            "unit_ids": [unit_id],
            "port_id": "example-enroll",
            "borrow": False,
            "purpose": "bind",
            "ttl_seconds": 120,
        },
    )
    sessions = (await snapshot(rust_backend))["sessions"]
    assert isinstance(sessions, list) and len(sessions) == 1
    session = sessions[0]
    assert isinstance(session, dict)
    assert session["state"] == "active" and session["borrow_state"] == "not_borrowed"
    assert session["port_id"] == "example-enroll"
