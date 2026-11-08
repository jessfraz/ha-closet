"""Authenticated, allowlisted HA bridge to the configured Closet service."""

import re
from urllib.parse import urlencode

from aiohttp import ClientError, web
from homeassistant.config_entries import ConfigEntryState
from homeassistant.core import HomeAssistant
from homeassistant.helpers.http import HomeAssistantView

from .client import Actor, ResponseTooLarge
from .const import DOMAIN, MAX_REQUEST_BYTES
from .coordinator import ClosetCoordinator

_ITEM_ID = r"(?!\.{1,2}$)[A-Za-z0-9_.:-]{1,200}"
# Accept bounded 7168-character cursors with room below HA's 8190-byte line.
_MAX_QUERY_CHARS = 8000
_ROUTES = (
    (re.compile(r"system|today"), frozenset({"GET"})),
    (re.compile(r"items"), frozenset({"GET"})),
    (re.compile(rf"items/{_ITEM_ID}"), frozenset({"GET"})),
    (re.compile(r"inventory/sync|inventory/validate"), frozenset({"POST"})),
    (re.compile(r"today/generate"), frozenset({"POST"})),
)
_LIST_QUERY = {
    "q",
    "brand",
    "category",
    "source",
    "confirmed_only",
    "limit",
    "page_token",
}


def loaded_coordinator(hass: HomeAssistant) -> ClosetCoordinator | None:
    """Resolve the loaded entry on each call; never retain unloaded state."""
    for entry in hass.config_entries.async_entries(DOMAIN):
        if entry.state is ConfigEntryState.LOADED:
            return entry.runtime_data
    return None


class ClosetView(HomeAssistantView):
    """User tokens authenticate to HA, never directly to the app."""

    url = "/api/closet/v1/{path:.*}"
    name = "api:closet:v1"
    requires_auth = True

    def __init__(self, hass: HomeAssistant) -> None:
        self._hass = hass

    async def _proxy(self, request: web.Request, path: str) -> web.Response:
        route = next((route for route in _ROUTES if route[0].fullmatch(path)), None)
        if route is None:
            raise web.HTTPNotFound
        if request.method not in route[1]:
            raise web.HTTPMethodNotAllowed(request.method, route[1])
        allowed_query = _LIST_QUERY if path == "items" else set()
        if (
            len(request.query_string) > _MAX_QUERY_CHARS
            or request.query.keys() - allowed_query
        ):
            raise web.HTTPBadRequest(reason="Unsupported query parameters")
        user = request["hass_user"]
        if not user.is_active or (request.method != "GET" and not user.is_admin):
            raise web.HTTPForbidden
        coordinator = loaded_coordinator(self._hass)
        if coordinator is None:
            raise web.HTTPServiceUnavailable
        payload = bytearray()
        async for chunk in request.content.iter_chunked(16 * 1024):
            payload.extend(chunk)
            if len(payload) > MAX_REQUEST_BYTES:
                raise web.HTTPRequestEntityTooLarge(
                    max_size=MAX_REQUEST_BYTES, actual_size=len(payload)
                )
        headers = {
            key: request.headers[key]
            for key in ("If-Match", "If-None-Match", "Idempotency-Key")
            if key in request.headers
        }
        try:
            response = await coordinator.client.request(
                request.method,
                f"/v1/{path}"
                + (
                    f"?{urlencode(list(request.query.items()))}"
                    if request.query
                    else ""
                ),
                body=bytes(payload) if payload else None,
                headers=headers,
                actor=Actor(user.id, user.name or "Home Assistant user", user.is_admin),
            )
        except TimeoutError:
            raise web.HTTPGatewayTimeout from None
        except ClientError, ResponseTooLarge:
            raise web.HTTPBadGateway from None
        if request.method != "GET" and response.status < 300:
            entry = coordinator.config_entry
            if entry is not None:
                entry.async_create_background_task(
                    self._hass,
                    coordinator.async_request_refresh(),
                    "Closet refresh after action",
                )
        return web.Response(
            status=response.status, body=response.body, headers=response.headers
        )

    get = _proxy
    post = _proxy
