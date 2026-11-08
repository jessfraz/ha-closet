"""Run Home Assistant against the real Rust service and disposable SQLite."""

import asyncio
import os
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

import pytest
from aiohttp import ClientError, ClientSession, ClientTimeout, web
from homeassistant.auth.const import GROUP_ID_USER
from homeassistant.auth.providers import AuthProvider
from homeassistant.core import HomeAssistant
from homeassistant.util.json import JsonObjectType
from pytest_aiohttp.plugin import AiohttpServer
from pytest_homeassistant_custom_component.common import MockUser

OWNER_TOKEN = "test-closet-owner-credential-at-least-32-characters"
INTEGRATION_TOKEN = "test-closet-integration-credential-at-least-32-characters"


@dataclass
class HAProtocol:
    """The real HTTP service boundary, with a fixed fictional model answer."""

    origin: str = ""
    generations: int = 0
    forecasts: int = 0

    async def weather_state(self, request: web.Request) -> web.Response:
        return web.json_response(
            {
                "state": "sunny",
                # A daily sample is always partial, including near midnight.
                "attributes": {"supported_features": 1, "temperature_unit": "°C"},
            }
        )

    async def forecast(self, request: web.Request) -> web.Response:
        self.forecasts += 1
        return web.json_response(
            {
                "service_response": {
                    "weather.example": {
                        "forecast": [
                            {
                                "datetime": datetime.now(UTC).isoformat(),
                                "condition": "sunny",
                                "temperature": 21.0,
                            }
                        ]
                    }
                }
            }
        )

    async def generate(self, request: web.Request) -> web.Response:
        self.generations += 1
        return web.json_response(
            {
                "service_response": {
                    "data": {
                        "separates_suggestions": [
                            {
                                "label": "Example combination",
                                "top_id": "example-top",
                                "bottom_id": "example-jeans",
                                "footwear_id": "example-flats",
                                "extra_ids": [],
                                "advisory_reason": "A fictional everyday combination.",
                            }
                        ]
                    }
                }
            }
        )


@pytest.fixture
async def ha_protocol(
    aiohttp_server: AiohttpServer, socket_enabled: None
) -> HAProtocol:
    """Never contact a real AI provider in PR tests."""
    protocol = HAProtocol()
    app = web.Application()
    app.router.add_get("/api/states/weather.example", protocol.weather_state)
    app.router.add_post("/api/services/weather/get_forecasts", protocol.forecast)
    app.router.add_post("/api/services/ai_task/generate_data", protocol.generate)
    server = await aiohttp_server(app)
    protocol.origin = str(server.make_url("/")).rstrip("/")
    return protocol


@pytest.fixture(autouse=True)
def auto_enable_custom_integrations(enable_custom_integrations: None) -> None:
    """Load the integration under its real domain."""


@pytest.fixture
async def hass_user(hass: HomeAssistant, local_auth: AuthProvider) -> MockUser:
    """A real authenticated HA user without administrator authority."""
    group = await hass.auth.async_get_group(GROUP_ID_USER)
    assert group is not None
    return MockUser(groups=[group]).add_to_hass(hass)


@pytest.fixture
async def hass_user_token(hass: HomeAssistant, hass_user: MockUser) -> str:
    """Use HA's token machinery rather than forged user headers."""
    refresh_token = await hass.auth.async_create_refresh_token(
        hass_user, client_id="https://example.invalid"
    )
    return hass.auth.async_create_access_token(refresh_token)


def catalog_item(
    item_id: str = "example-tee",
    *,
    name: str = "Example cotton tee",
    category: str = "clothing.top",
    confirmed: bool = False,
) -> JsonObjectType:
    """Invented catalog data, not a household or merchant fixture."""
    return {
        "id": item_id,
        "brand": "Example label",
        "name": name,
        "category": category,
        "color": "Cool white",
        "size": None,
        "quantity": 1,
        "material": None,
        "sku": None,
        "purchase_source": None,
        "current_ownership_confirmed": confirmed,
        "images": [],
        "notes": [],
    }


@dataclass(frozen=True, slots=True)
class RustBackend:
    """A running service; fake test credentials never appear in representations."""

    origin: str
    owner_token: str = field(default=OWNER_TOKEN, repr=False)
    integration_token: str = field(default=INTEGRATION_TOKEN, repr=False)

    async def sync(
        self, records: list[JsonObjectType], key: str = "fixture-sync"
    ) -> None:
        """Seed through the real API, including its revision/idempotency guards."""
        async with ClientSession() as client:
            async with client.get(
                f"{self.origin}/v1/system",
                headers={"Authorization": f"Bearer {self.owner_token}"},
            ) as response:
                assert response.status == 200
                data = await response.json()
                revision = data["catalog_revision"]
            async with client.post(
                f"{self.origin}/v1/inventory/sync",
                headers={
                    "Authorization": f"Bearer {self.owner_token}",
                    "If-Match": f'"{revision}"',
                    "Idempotency-Key": key,
                },
                json={"items": records, "dry_run": False, "allow_empty": False},
            ) as response:
                assert response.status == 200, await response.text()


@pytest.fixture
async def rust_backend(
    hass: HomeAssistant,
    tmp_path: Path,
    unused_tcp_port_factory: Callable[[], int],
    socket_enabled: None,
    request: pytest.FixtureRequest,
    ha_protocol: HAProtocol,
) -> AsyncIterator[RustBackend]:
    """Start and stop the binary, never a reimplementation of its domain logic."""
    # Reserve after socket_enabled has loaded, not in a sibling port fixture
    # that newer HA test plugins can evaluate before the network permission.
    unused_tcp_port = unused_tcp_port_factory()
    configured_binary = Path(
        os.environ.get("CLOSET_TEST_BINARY", "target/debug/closet")
    )
    binary = await asyncio.to_thread(configured_binary.resolve)
    if not await asyncio.to_thread(binary.is_file):
        pytest.fail("Build the backend first: cargo build --locked --bin closet")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("CLOSET_", "SUPERVISOR"))
    }
    environment.update(
        CLOSET_API_TOKEN=OWNER_TOKEN,
        CLOSET_INTEGRATION_TOKEN=INTEGRATION_TOKEN,
        CLOSET_DATABASE=str(tmp_path / "catalog.sqlite3"),
        CLOSET_BIND=f"127.0.0.1:{unused_tcp_port}",
        CLOSET_TIMEZONE="UTC",
    )
    if getattr(request, "param", False):
        environment.update(
            CLOSET_HA_URL=ha_protocol.origin,
            CLOSET_HA_TOKEN="test-closet-upstream-credential",
            CLOSET_WEATHER_ENTITY="weather.example",
            CLOSET_AI_TASK_ENTITY="ai_task.example",
        )
    process = await asyncio.create_subprocess_exec(
        str(binary),
        env=environment,
        stdout=asyncio.subprocess.DEVNULL,
        stderr=asyncio.subprocess.PIPE,
    )
    backend = RustBackend(f"http://127.0.0.1:{unused_tcp_port}")
    try:
        async with asyncio.timeout(15), ClientSession() as client:
            while True:
                if process.returncode is not None:
                    pytest.fail("The Rust service exited during startup")
                try:
                    async with client.get(
                        f"{backend.origin}/health", timeout=ClientTimeout(total=1)
                    ) as response:
                        if response.status == 200:
                            break
                except ClientError, TimeoutError:
                    pass
                await asyncio.sleep(0.02)
        yield backend
    finally:
        if process.returncode is None:
            process.terminate()
            try:
                async with asyncio.timeout(5):
                    await process.wait()
            except TimeoutError:
                process.kill()
                await process.wait()
