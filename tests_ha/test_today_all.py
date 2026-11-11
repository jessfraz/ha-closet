"""Occasion groups pass through authenticated HA and its actual AI schema path."""

import asyncio

import pytest
from homeassistant.core import HomeAssistant
from pytest_homeassistant.typing import ClientSessionGenerator

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_ha.test_integration import PREFIX, connect


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
@pytest.mark.parametrize("scope", ["day", "sleep", "both"])
async def test_all_through_real_ha_skips_only_ineligible_groups_and_replays_once(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
    scope: str,
) -> None:
    records = []
    if scope != "sleep":
        records.extend(
            [
                catalog_item("example-top"),
                catalog_item("example-jeans", category="clothing.jeans"),
                catalog_item("example-flats", category="footwear"),
            ]
        )
    if scope != "day":
        records.append(
            catalog_item("example-nightgown", category="clothing.sleepwear.nightgown")
        )
    await rust_backend.sync(records)
    await connect(hass, rust_backend)
    client = await hass_client()
    assert ha_protocol.generations == 0
    key = f"all-groups-{scope}"
    inputs = {"occasion": "all", "weather_mode": "ignored"}
    accepted = await client.post(
        f"{PREFIX}/today/generate", json=inputs, headers={"Idempotency-Key": key}
    )
    assert accepted.status == 202, await accepted.text()
    generation = await accepted.json()
    async with asyncio.timeout(5):
        while True:
            response = await client.get(f"{PREFIX}/today")
            assert response.status == 200
            data = await response.json()
            if data["generation"]["status"] != "running":
                break
            await asyncio.sleep(0.01)
    assert data["generation"]["status"] == "succeeded", data
    expected = {"everyday", "work", "dinner"} if scope != "sleep" else set()
    if scope != "day":
        expected.add("bedtime")
    assert data["result"]["inputs"] == inputs
    assert {
        suggestion["occasion"] for suggestion in data["result"]["suggestions"]
    } == expected
    assert ha_protocol.last_schema is not None
    properties = ha_protocol.last_schema["properties"]
    assert isinstance(properties, dict)
    assert set(properties) == expected
    for suggestion in data["result"]["suggestions"]:
        if suggestion["occasion"] == "bedtime":
            assert suggestion["item_ids"] == ["example-nightgown"]
        else:
            assert suggestion["item_ids"] == [
                "example-top",
                "example-jeans",
                "example-flats",
            ]
    replay = await client.post(
        f"{PREFIX}/today/generate", json=inputs, headers={"Idempotency-Key": key}
    )
    assert replay.status == 202
    assert (await replay.json())["id"] == generation["id"]
    assert ha_protocol.generations == 1
