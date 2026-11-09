"""Use requirements and complete copy summaries cross the real authenticated HA API."""

import asyncio
import json

import pytest
from homeassistant.core import HomeAssistant
from homeassistant.util.json import JsonObjectType
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from tests_ha.conftest import HAProtocol, RustBackend, catalog_item
from tests_ha.test_integration import PREFIX, connect
from tests_ha.tracking_fixture import command, configure, snapshot, units


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
@pytest.mark.parametrize("required", [False, True])
async def test_steel_toe_requirement_is_explicit_and_all_omits_bedtime_when_checked(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
    required: bool,
) -> None:
    selected = catalog_item("example-flats", category="footwear.boots")
    excluded = catalog_item("example-excluded-footwear", category="footwear.shoes")
    safety: JsonObjectType = {"kind": "safety_work_only", "requirement": "steel_toe"}
    (selected if required else excluded)["usage"] = safety
    selected["price"] = None
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            selected,
            excluded,
            catalog_item("example-nightgown", category="clothing.sleepwear.nightgown"),
        ]
    )
    await connect(hass, rust_backend)
    client = await hass_client()
    suffix = "?footwear_requirement=steel_toe" if required else ""
    inputs = {"occasion": "all", "weather_mode": "ignored"}
    if required:
        inputs["footwear_requirement"] = "steel_toe"
    accepted = await client.post(
        f"{PREFIX}/today/generate",
        json=inputs,
        headers={"Idempotency-Key": "example-requirement"},
    )
    assert accepted.status == 202, await accepted.text()
    generation = await accepted.json()
    assert generation["inputs"] == inputs
    async with asyncio.timeout(5):
        while True:
            response = await client.get(f"{PREFIX}/today{suffix}")
            assert response.status == 200
            data = await response.json()
            if data["generation"]["status"] != "running":
                break
            await asyncio.sleep(0.01)
    assert data["generation"]["status"] == "succeeded", data
    assert data["result"]["inputs"] == inputs
    expected = {"everyday", "work", "dinner"} | (set() if required else {"bedtime"})
    assert {outfit["occasion"] for outfit in data["result"]["suggestions"]} == expected
    assert ha_protocol.last_context is not None
    assert "example-excluded-footwear" not in json.dumps(ha_protocol.last_context)
    if required:
        assert "example-nightgown" not in json.dumps(ha_protocol.last_context)
        normal = await (await client.get(f"{PREFIX}/today")).json()
        assert normal["result"] is None and normal["generation"] is None
    item = await (await client.get(f"{PREFIX}/items/example-flats")).json()
    assert item["price"] is None
    if required:
        assert item["usage"] == safety
    replay = await client.post(
        f"{PREFIX}/today/generate",
        json=inputs,
        headers={"Idempotency-Key": "example-requirement"},
    )
    assert replay.status == 202 and (await replay.json())["id"] == generation["id"]
    assert ha_protocol.generations == 1
    bad = await client.post(
        f"{PREFIX}/today/generate",
        json={"occasion": "bedtime", "footwear_requirement": "steel_toe"},
        headers={"Idempotency-Key": "example-bedtime"},
    )
    assert bad.status == 400
    assert ha_protocol.generations == 1


@pytest.mark.parametrize("rust_backend", [True], indirect=True)
async def test_required_boots_in_laundry_never_fall_back_to_ordinary_shoes(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
    ha_protocol: HAProtocol,
) -> None:
    boots = catalog_item("example-safety-boots", category="footwear.boots")
    boots["usage"] = {"kind": "safety_work_only", "requirement": "steel_toe"}
    await rust_backend.sync(
        [
            catalog_item("example-top"),
            catalog_item("example-jeans", category="clothing.jeans"),
            catalog_item("example-flats", category="footwear"),
            boots,
        ]
    )
    await configure(rust_backend)
    boot = next(
        unit
        for unit in units(await snapshot(rust_backend))
        if unit["catalog_id"] == "example-safety-boots"
    )
    await command(
        rust_backend,
        {"type": "baseline", "unit_id": boot["id"], "area_id": "example-closet"},
    )
    await command(
        rust_backend,
        {
            "type": "correct",
            "unit_id": boot["id"],
            "status": "needs_laundry",
            "note": "Fictional laundry state",
        },
    )
    await connect(hass, rust_backend)
    client = await hass_client()
    accepted = await client.post(
        f"{PREFIX}/today/generate",
        json={
            "occasion": "work",
            "weather_mode": "ignored",
            "footwear_requirement": "steel_toe",
        },
        headers={"Idempotency-Key": "unavailable-safety-footwear"},
    )
    assert accepted.status == 202
    async with asyncio.timeout(5):
        while True:
            data = await (
                await client.get(f"{PREFIX}/today?footwear_requirement=steel_toe")
            ).json()
            if data["generation"]["status"] != "running":
                break
            await asyncio.sleep(0.01)
    assert data["generation"]["status"] == "failed"
    assert data["generation"]["error_code"] == "no_eligible_footwear"
    assert data["result"] is None and ha_protocol.generations == 0


async def test_complete_variant_copy_summaries_survive_the_authenticated_bridge(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    first = catalog_item("example-copy-a", name="Example shared shirt")
    second = catalog_item("example-copy-b", name="Example shared shirt")
    for record, size, quantity in [(first, "XS", 2), (second, "S", 1)]:
        record["identity"] = {
            "style_id": "example-shared-style",
            "variant_id": "example-shared-variant",
        }
        record["size"] = size
        record["quantity"] = quantity
    second["shop"] = {
        "id": "example-shop",
        "label": "Example shop",
        "season": None,
        "years": [],
    }
    await rust_backend.sync([first, second])
    await connect(hass, rust_backend)
    client = await hass_client()
    response = await client.get(f"{PREFIX}/items?limit=1&source=shop%3Aexample-shop")
    assert response.status == 200
    data = await response.json()
    assert len(data["items"]) == 1
    assert len(data["variant_groups"]) == 1
    group = data["variant_groups"][0]
    assert group["quantity"] == 3
    assert [copy["id"] for copy in group["copies"]] == [
        "example-copy-a",
        "example-copy-b",
    ]
    assert [copy["size"] for copy in group["copies"]] == ["XS", "S"]
