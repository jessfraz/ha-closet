"""Actual revision-bound Rust pages, including controlled concurrent writes."""

from aiohttp import ClientSession
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from pytest_homeassistant_custom_component.common import MockConfigEntry

from custom_components.closet.client import Actor, BackendResponse, ClosetClient
from custom_components.closet.const import DOMAIN
from custom_components.closet.coordinator import ClosetCoordinator

from .conftest import RustBackend, catalog_item
from .tracking_fixture import command, configure, snapshot, units


class RacingClient(ClosetClient):
    """Orchestrate real mutations between requests, never fabricate responses."""

    def __init__(
        self, session: ClientSession, backend: RustBackend, unit_id: str
    ) -> None:
        super().__init__(session, backend.origin, backend.integration_token)
        self.backend = backend
        self.unit_id = unit_id
        self.races = 0
        self.starts = 0
        self.paused = False

    async def request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        actor: Actor | None = None,
        headers: dict[str, str] | None = None,
    ) -> BackendResponse:
        """The production client still performs every request and decodes its bytes."""
        if path.startswith("/v1/tracking/projection?"):
            if "page_token=" not in path:
                self.starts += 1
            elif self.races:
                self.paused = not self.paused
                await command(
                    self.backend,
                    {"type": "pause", "unit_id": self.unit_id, "paused": self.paused},
                )
                if self.races > 0:
                    self.races -= 1
        return await super().request(
            method, path, body=body, actor=actor, headers=headers
        )


async def test_projection_retries_once_and_never_publishes_partial_units(
    hass: HomeAssistant,
    rust_backend: RustBackend,
) -> None:
    records = [catalog_item("page-example-one"), catalog_item("page-example-two")]
    for record in records:
        record["quantity"] = 300
    await rust_backend.sync(records)
    await configure(rust_backend)
    physical = units(await snapshot(rust_backend))
    first_id, last_id = physical[0]["id"], physical[-1]["id"]
    assert isinstance(first_id, str) and isinstance(last_id, str)
    for unit_id in [first_id, last_id]:
        await command(
            rust_backend,
            {"type": "baseline", "unit_id": unit_id, "area_id": "example-closet"},
        )
    client = RacingClient(async_get_clientsession(hass), rust_backend, first_id)
    version = (await client.request("GET", "/v1/system")).json()["version"]
    assert isinstance(version, str)
    entry = MockConfigEntry(domain=DOMAIN)
    entry.add_to_hass(hass)
    coordinator = ClosetCoordinator(hass, entry, client, version)
    try:
        await coordinator.async_refresh()
        tracking = coordinator.data.tracking
        assert coordinator.last_update_success and tracking is not None
        assert len(tracking.units) == 600
        assert {unit.id for unit in tracking.units if unit.ready} == {first_id, last_id}
        client.races, client.starts = 1, 0
        await coordinator.async_refresh()
        tracking = coordinator.data.tracking
        assert coordinator.last_update_success and tracking is not None
        assert client.starts == 2
        assert len(tracking.units) == len({unit.id for unit in tracking.units}) == 600
        assert next(unit for unit in tracking.units if unit.id == first_id).paused
        assert {unit.id for unit in tracking.units if unit.ready} == {first_id, last_id}
        previous = coordinator.data
        client.races, client.starts = -1, 0
        await coordinator.async_refresh()
        assert not coordinator.last_update_success
        assert client.starts == 2
        assert coordinator.data is previous
        client.races = 0
        await coordinator.async_refresh()
        assert coordinator.last_update_success
        tracking = coordinator.data.tracking
        assert tracking is not None and len(tracking.units) == 600
    finally:
        await coordinator.async_shutdown()
