"""Real HA flows, scoped HTTP bridge, entities and Assist against Rust."""

import asyncio
import json
import os
from pathlib import Path
from urllib.parse import urlencode
from uuid import uuid4

import probatio
import pytest
from aiohttp import ClientSession
from homeassistant.components.homeassistant.exposed_entities import async_expose_entity
from homeassistant.config_entries import (
    SOURCE_HASSIO,
    SOURCE_REAUTH,
    SOURCE_USER,
    ConfigEntryState,
)
from homeassistant.core import Context, HomeAssistant
from homeassistant.data_entry_flow import FlowResultType
from homeassistant.helpers import entity_registry as er
from homeassistant.helpers import llm
from homeassistant.helpers.service_info.hassio import HassioServiceInfo
from homeassistant.setup import async_setup_component
from pytest_homeassistant_custom_component.common import MockConfigEntry, MockUser
from pytest_homeassistant_custom_component.typing import ClientSessionGenerator

from custom_components.closet import ClosetConfigEntry
from custom_components.closet.const import CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from custom_components.closet.diagnostics import async_get_config_entry_diagnostics

from .conftest import RustBackend, catalog_item

PREFIX = "/api/closet/v1"


async def connect(hass: HomeAssistant, backend: RustBackend) -> ClosetConfigEntry:
    """Use the actual standalone flow, not a hand-created runtime coordinator."""
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "standalone"}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: backend.origin, CONF_TOKEN: backend.integration_token},
    )
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()
    entry = result["result"]
    assert entry.state is ConfigEntryState.LOADED
    return entry


async def test_flow_counts_diagnostics_and_unload(
    hass: HomeAssistant, hass_client: ClientSessionGenerator, rust_backend: RustBackend
) -> None:
    await rust_backend.sync([catalog_item()])
    entry = await connect(hass, rust_backend)
    registry = er.async_get(hass)
    item_entity = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_item_count"
    )
    assert item_entity is not None
    item_state = hass.states.get(item_entity)
    assert item_state is not None and item_state.state == "1"
    diagnostics = await async_get_config_entry_diagnostics(hass, entry)
    assert diagnostics["item_count"] == 1
    assert set(diagnostics) == {
        "integration_version",
        "available",
        "backend_version",
        "item_count",
        "archived_count",
        "today_configured",
    }
    rendered = json.dumps(diagnostics)
    assert "example-tee" not in rendered
    assert "Example cotton tee" not in rendered
    assert rust_backend.integration_token not in rendered
    assert rust_backend.origin not in rendered
    client = await hass_client()
    assert (await client.get(f"{PREFIX}/items")).status == 200
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert (await client.get(f"{PREFIX}/items")).status == 503
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    reloaded_state = hass.states.get(item_entity)
    assert reloaded_state is not None and reloaded_state.state == "1"


async def test_upgrade_removes_only_the_retired_entry_owned_sensor(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    await rust_backend.sync([catalog_item()])
    entry = await connect(hass, rust_backend)
    hass.config_entries.async_update_entry(entry, options={"preserved_setting": "keep"})
    original_data = dict(entry.data)
    registry = er.async_get(hass)
    item_id = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_item_count"
    )
    archived_id = registry.async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_archived_count"
    )
    assert item_id is not None and archived_id is not None
    registry.async_update_entity(item_id, name="Custom inventory total")
    obsolete = registry.async_get_or_create(
        "sensor", DOMAIN, f"{entry.entry_id}_confirmed_count", config_entry=entry
    )
    other = registry.async_get_or_create(
        "sensor", DOMAIN, f"{entry.entry_id}_unrelated_metric", config_entry=entry
    )
    assert await hass.config_entries.async_unload(entry.entry_id)
    assert await hass.config_entries.async_setup(entry.entry_id)
    await hass.async_block_till_done()
    assert registry.async_get(obsolete.entity_id) is None
    retained = registry.async_get(item_id)
    assert retained is not None and retained.name == "Custom inventory total"
    assert registry.async_get(archived_id) is not None
    assert registry.async_get(other.entity_id) is not None
    assert dict(entry.data) == original_data
    assert dict(entry.options) == {"preserved_setting": "keep"}
    state = hass.states.get(item_id)
    assert state is not None and state.state == "1"


async def test_admin_sync_and_untrusted_actor_headers(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    entry = await connect(hass, rust_backend)
    user = await hass_client(hass_user_token)
    body = {"items": [catalog_item()], "dry_run": False, "allow_empty": False}
    headers = {
        "If-Match": '"0"',
        "Idempotency-Key": "ha-catalog-sync",
        "X-Closet-Actor-Id": "ha:forged",
        "X-Closet-Admin": "true",
    }
    assert (
        await user.post(f"{PREFIX}/inventory/sync", json=body, headers=headers)
    ).status == 403
    assert (await user.post(f"{PREFIX}/today/generate", json={})).status == 403
    admin = await hass_client()
    applied = await admin.post(f"{PREFIX}/inventory/sync", json=body, headers=headers)
    assert applied.status == 200, await applied.text()
    await hass.async_block_till_done()
    data = await (await user.get(f"{PREFIX}/items")).json()
    assert data["total"] == 1
    assert data["items"][0]["id"] == "example-tee"
    assert entry.runtime_data.data.item_count == 1
    async with ClientSession() as native:
        response = await native.post(
            f"{rust_backend.origin}/v1/inventory/sync",
            json=body,
            headers={
                "Authorization": f"Bearer {rust_backend.integration_token}",
                "If-Match": '"1"',
                "Idempotency-Key": "unattributed",
            },
        )
        assert response.status == 403


async def test_proxy_reads_every_valid_leading_item_character(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    """The HA read surface accepts the catalog validator's whole ID alphabet."""
    identifiers = ["_example", "-example", ".example", ":example", "a" * 200]
    await rust_backend.sync([catalog_item(item_id) for item_id in identifiers])
    await connect(hass, rust_backend)
    client = await hass_client()
    for item_id in identifiers:
        response = await client.get(f"{PREFIX}/items/{item_id}")
        assert response.status == 200, await response.text()
        assert (await response.json())["id"] == item_id
    for invalid in [".", "..", "x" * 201, "not!an!id"]:
        assert (await client.get(f"{PREFIX}/items/{invalid}")).status == 404


async def test_proxy_filters_sources_and_returns_global_facets(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """Authenticated reads preserve source identity, precedence and paging."""
    records = [catalog_item(f"source-example-{index}") for index in range(4)]
    records[0].update(
        shop={
            "id": "demo-atelier",
            "label": "Demo atelier",
            "season": "Spring/Summer",
            "years": [],
        },
        purchase_source="self_bought",
    )
    records[1]["shop"] = {
        "id": "demo-atelier",
        "label": "Demo atelier",
        "season": "Fall/Winter",
        "years": [],
    }
    records[2]["purchase_source"] = "self_bought"
    records[3]["purchase_source"] = "Example gift"
    await rust_backend.sync(records)
    await connect(hass, rust_backend)
    client = await hass_client(hass_user_token)
    response = await client.get(
        f"{PREFIX}/items", params={"source": "shop:demo-atelier", "limit": "1"}
    )
    assert response.status == 200, await response.text()
    first = await response.json()
    assert first["total"] == 2
    assert first["scope_total"] == 4
    assert {
        facet["source"]: facet["count"] for facet in first["facets"]["sources"]
    } == {
        "shop:demo-atelier": 2,
        "purchase:self_bought": 1,
        "purchase:Example gift": 1,
    }
    assert first["items"][0]["shop"]["season"] == "Spring/Summer"
    page = await client.get(
        f"{PREFIX}/items",
        params={
            "source": "shop:demo-atelier",
            "limit": "1",
            "page_token": first["next_page"],
        },
    )
    assert page.status == 200, await page.text()
    second = await page.json()
    assert second["items"][0]["id"] != first["items"][0]["id"]
    assert second["items"][0]["shop"]["season"] == "Fall/Winter"
    bought = await (
        await client.get(f"{PREFIX}/items", params={"source": "purchase:self_bought"})
    ).json()
    assert [item["id"] for item in bought["items"]] == ["source-example-2"]
    gifted = await (
        await client.get(f"{PREFIX}/items", params={"source": "purchase:Example gift"})
    ).json()
    assert [item["id"] for item in gifted["items"]] == ["source-example-3"]
    rejected = await client.get(f"{PREFIX}/items", params={"source": "unspecified"})
    assert rejected.status == 400


async def test_missing_source_is_rejected_offline_and_ha_sync_is_atomic(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    original = catalog_item("source-existing", name="Original example")
    await rust_backend.sync([original])
    await connect(hass, rust_backend)
    changed = {**original, "name": "Changed example"}
    missing = catalog_item("source-missing")
    missing["purchase_source"] = None
    records = [changed, missing]
    backend_binary = await asyncio.to_thread(
        Path(os.environ.get("CLOSET_TEST_BINARY", "target/debug/closet")).resolve
    )
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("CLOSET_", "HOME_ASSISTANT_", "SUPERVISOR"))
    }
    process = await asyncio.create_subprocess_exec(
        str(backend_binary.with_name("ha-closet")),
        "inventory",
        "validate",
        "--input",
        "-",
        env=environment,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(15):
            output, error = await process.communicate(json.dumps(records).encode())
    finally:
        if process.returncode is None:
            process.kill()
            await process.wait()
    assert process.returncode != 0
    assert not output
    diagnostic = json.loads(error)
    assert diagnostic["status"] is None
    assert diagnostic["error_code"] == "invalid_catalog"
    assert any(
        field["index"] == 1 and field["code"] == "source_required"
        for field in diagnostic["errors"]
    )
    client = await hass_client()
    validated = await client.post(f"{PREFIX}/inventory/validate", json=records)
    assert validated.status == 400
    assert (await validated.json())["error_code"] == "invalid_catalog"
    rejected = await client.post(
        f"{PREFIX}/inventory/sync",
        json={"items": records, "dry_run": False, "allow_empty": False},
        headers={"If-Match": '"1"', "Idempotency-Key": "missing-source-must-not-apply"},
    )
    assert rejected.status == 400
    assert (await rejected.json())["error_code"] == "invalid_catalog"
    projection = await (await client.get(f"{PREFIX}/items")).json()
    assert projection["catalog_revision"] == 1
    assert projection["total"] == 1
    assert projection["items"][0]["id"] == "source-existing"
    assert projection["items"][0]["name"] == "Original example"


async def test_proxy_pages_maximum_escaped_filters_with_cursor_only(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    """HTTP and the actual CLI page legal filters through the real HA bridge."""
    records = [
        catalog_item("a" * 199 + suffix, name='"' * 512, category="a" * 200)
        for suffix in ("0", "1")
    ]
    for record in records:
        record.update(brand='"' * 1024, purchase_source='"' * 1024)
    await rust_backend.sync(records)
    await connect(hass, rust_backend)
    client = await hass_client(hass_user_token)
    source = "purchase:" + '"' * 1024
    for label, filters in (
        ("source-only", {"source": source}),
        (
            "combined",
            {
                "source": source,
                "brand": '"' * 1024,
                "q": '"' * 256,
                "category": "a" * 200,
            },
        ),
    ):
        response = await client.get(f"{PREFIX}/items", params={**filters, "limit": "1"})
        assert response.status == 200, await response.text()
        first = await response.json()
        assert first["total"] == 2
        cursor = first["next_page"]
        continuation = {"page_token": cursor, "limit": "1"}
        print(f"HA {label}: token={len(cursor)} query={len(urlencode(continuation))}")
        following = await client.get(f"{PREFIX}/items", params=continuation)
        assert following.status == 200, await following.text()
        second = await following.json()
        assert second["total"] == 2
        assert first["items"][0]["id"] != second["items"][0]["id"]
        assert second["next_page"] is None

    backend_binary = await asyncio.to_thread(
        Path(os.environ.get("CLOSET_TEST_BINARY", "target/debug/closet")).resolve
    )
    cli_binary = backend_binary.with_name("ha-closet")
    assert await asyncio.to_thread(cli_binary.is_file), (
        "Build both binaries: cargo build --bins"
    )
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("CLOSET_", "HOME_ASSISTANT_", "SUPERVISOR"))
    }
    environment.update(
        HOME_ASSISTANT_URL=str(client.make_url("/")).rstrip("/"),
        HOME_ASSISTANT_TOKEN=hass_user_token,
    )

    async def run_cli(*arguments: str) -> tuple[int, str, str]:
        process = await asyncio.create_subprocess_exec(
            str(cli_binary),
            "--transport",
            "home-assistant",
            "--timeout",
            "10",
            "items",
            "list",
            *arguments,
            env=environment,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            async with asyncio.timeout(15):
                stdout, stderr = await process.communicate()
        finally:
            if process.returncode is None:
                process.kill()
                await process.wait()
        assert process.returncode is not None
        return process.returncode, stdout.decode(), stderr.decode()

    filters = (
        "--source",
        source,
        "--brand",
        '"' * 1024,
        "--q",
        '"' * 256,
        "--category",
        "a" * 200,
    )
    code, output, error = await run_cli(*filters, "--limit", "1")
    assert code == 0, error
    assert not error
    cli_first = json.loads(output)
    assert cli_first["status"] == 200
    assert cli_first["data"]["total"] == 2
    cursor = cli_first["data"]["next_page"]
    # The previously documented repeated-filter pattern is rejected locally,
    # rather than emitting a request line larger than HA can accept.
    code, output, error = await run_cli(
        *filters, "--page-token", cursor, "--limit", "1"
    )
    assert code == 2, error
    assert not output
    assert json.loads(error)["error_code"] == "invalid_arguments"
    code, output, error = await run_cli(
        "--page-token", cursor, "--limit", "1", "--format", "json"
    )
    assert code == 0, error
    assert not error
    cli_second = json.loads(output)
    assert cli_second["status"] == 200
    assert cli_second["data"]["total"] == 2
    assert cli_second["data"]["next_page"] is None
    assert cli_first["data"]["items"][0]["id"] != cli_second["data"]["items"][0]["id"]


async def test_proxy_sync_accepts_valid_catalog_plus_request_envelope(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
) -> None:
    """A valid near-1-MiB catalog still fits after sync adds its envelope."""
    records = [catalog_item(f"large-example-{index}") for index in range(4)]
    for record in records:
        record.update(price=None, shop=None)
        record["notes"] = ["x" * 2614 for _ in range(100)]
    # Match the Rust compact catalog size, leaving room for no more than 16 bytes.
    target_bytes = 1_048_560
    encoded = json.dumps(records, separators=(",", ":"))
    remaining = target_bytes - len(encoded.encode())
    assert 0 <= remaining <= 4096 - 2614
    records[-1]["notes"] = ["x" * (2614 + remaining)] + ["x" * 2614] * 99
    catalog = json.dumps(records, separators=(",", ":"))
    assert len(catalog.encode()) == target_bytes
    await connect(hass, rust_backend)
    client = await hass_client()
    validated = await client.post(
        f"{PREFIX}/inventory/validate",
        data=catalog,
        headers={"Content-Type": "application/json"},
    )
    assert validated.status == 200, await validated.text()
    body = '{"items":' + catalog + ',"dry_run":false,"allow_empty":false}'
    assert len(body.encode()) > 1_048_576
    synced = await client.post(
        f"{PREFIX}/inventory/sync",
        data=body,
        headers={
            "Content-Type": "application/json",
            "If-Match": '"0"',
            "Idempotency-Key": "large-ha-catalog-sync",
        },
    )
    assert synced.status == 200, await synced.text()
    listing = await (await client.get(f"{PREFIX}/items")).json()
    assert listing["total"] == 4


@pytest.mark.parametrize(
    ("method", "path", "status"),
    [
        ("get", "credentials", 404),
        ("post", "items", 405),
        ("get", "items?url=https://example.invalid", 400),
        ("get", "system?limit=1", 400),
        ("post", "today/generate?url=x", 400),
    ],
)
async def test_proxy_has_fixed_route_query_and_method_scope(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    rust_backend: RustBackend,
    method: str,
    path: str,
    status: int,
) -> None:
    await connect(hass, rust_backend)
    client = await hass_client()
    response = await getattr(client, method)(f"{PREFIX}/{path}")
    assert response.status == status


async def test_owner_token_cannot_be_saved_as_integration(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "standalone"}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {CONF_BACKEND_URL: rust_backend.origin, CONF_TOKEN: rust_backend.owner_token},
    )
    assert result["type"] is FlowResultType.FORM
    assert result["errors"] == {"base": "integration_required"}
    assert not hass.config_entries.async_entries(DOMAIN)


async def test_supervisor_discovery_requires_confirmation(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_HASSIO},
        data=HassioServiceInfo(
            config={
                CONF_BACKEND_URL: rust_backend.origin,
                CONF_TOKEN: rust_backend.integration_token,
            },
            name="Closet",
            slug="example_closet",
            uuid=uuid4().hex,
        ),
    )
    assert result["type"] is FlowResultType.FORM
    assert result["step_id"] == "hassio_confirm"
    assert not hass.config_entries.async_entries(DOMAIN)
    result = await hass.config_entries.flow.async_configure(result["flow_id"], {})
    assert result["type"] is FlowResultType.CREATE_ENTRY
    await hass.async_block_till_done()


async def test_reauth_replaces_credential_without_duplicate_entry(
    hass: HomeAssistant, rust_backend: RustBackend
) -> None:
    entry = MockConfigEntry(
        domain=DOMAIN,
        data={
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: "expired-example-token",
        },
    )
    entry.add_to_hass(hass)
    result = await hass.config_entries.flow.async_init(
        DOMAIN,
        context={"source": SOURCE_REAUTH, "entry_id": entry.entry_id},
        data=entry.data,
    )
    assert result["step_id"] == "reauth_confirm"
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"],
        {
            CONF_BACKEND_URL: rust_backend.origin,
            CONF_TOKEN: rust_backend.integration_token,
        },
    )
    assert result["type"] is FlowResultType.ABORT
    assert result["reason"] == "reauth_successful"
    await hass.async_block_till_done()
    assert entry.data[CONF_TOKEN] == rust_backend.integration_token
    assert len(hass.config_entries.async_entries(DOMAIN)) == 1


@pytest.mark.parametrize(
    "origin",
    [
        "http://user:secret@localhost",
        "http://localhost/path",
        "https://localhost?next=x",
        "http://localhost:0",
    ],
)
async def test_standalone_rejects_non_origin_before_request(
    hass: HomeAssistant, origin: str
) -> None:
    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": SOURCE_USER}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {"next_step_id": "standalone"}
    )
    result = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_BACKEND_URL: origin, CONF_TOKEN: "test-token"}
    )
    assert result["errors"] == {CONF_BACKEND_URL: "invalid_url"}


async def test_assist_reads_actual_inventory_and_respects_revoked_exposure(
    hass: HomeAssistant, rust_backend: RustBackend, hass_user: MockUser
) -> None:
    record = catalog_item()
    record["purchase_source"] = "excluded-example-provenance"
    record["notes"] = ["excluded-example-source-note"]
    await rust_backend.sync([record])
    entry = await connect(hass, rust_backend)
    entity_id = er.async_get(hass).async_get_entity_id(
        "sensor", DOMAIN, f"{entry.entry_id}_item_count"
    )
    assert entity_id is not None
    assert await async_setup_component(hass, "homeassistant", {})
    assert await async_setup_component(hass, "llm", {})
    context = llm.LLMContext(
        platform="test",
        context=Context(user_id=hass_user.id),
        language="en",
        assistant="conversation",
        device_id=None,
    )
    async_expose_entity(hass, "conversation", entity_id, False)
    api = await llm.async_get_api(hass, "assist", context)
    assert not {tool.name for tool in api.tools if tool.integration == DOMAIN}
    async_expose_entity(hass, "conversation", entity_id, True)
    api = await llm.async_get_api(hass, "assist", context)
    assert {tool.name for tool in api.tools if tool.integration == DOMAIN} == {
        "closet__query",
        "closet__today",
    }
    for tool in api.tools:
        if tool.integration != DOMAIN:
            continue
        schema = probatio.to_openapi(
            tool.parameters,
            custom_serializer=api.custom_serializer,
            openapi_version="3.1.0",
        )
        assert schema["type"] == "object"
        assert schema["additionalProperties"] is False
        json.dumps(schema)
    result = await api.async_call_tool(
        llm.ToolInput(tool_name="closet__query", tool_args={"q": "cotton"})
    )
    assert not result.error
    items = result.data.get("items")
    assert isinstance(items, list) and isinstance(items[0], dict)
    assert "size" not in items[0]
    assert "material" not in items[0]
    serialized = json.dumps(result.data)
    assert "example-tee" in serialized
    assert "excluded-example-provenance" not in serialized
    assert "excluded-example-source-note" not in serialized
    today = await api.async_call_tool(
        llm.ToolInput(tool_name="closet__today", tool_args={})
    )
    assert today.data["generation"] is None
    async_expose_entity(hass, "conversation", entity_id, False)
    revoked = await api.async_call_tool(
        llm.ToolInput(tool_name="closet__query", tool_args={})
    )
    assert revoked.error
    assert revoked.data == {"error": "not_exposed"}
