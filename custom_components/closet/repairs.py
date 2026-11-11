"""Native version repair for the Closet app and companion."""

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from ._app_kit.repairs import VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .client import ClosetClient
from .const import CONF_ADDON_SLUG, CONF_BACKEND_URL, CONF_TOKEN, DOMAIN
from .models import ClosetSummary


async def _async_probe_version(
    hass: HomeAssistant, entry: ConfigEntry
) -> ReleaseVersion:
    client = ClosetClient(
        async_get_clientsession(hass),
        entry.data[CONF_BACKEND_URL],
        entry.data[CONF_TOKEN],
    )
    response = await client.request("GET", "/v1/system")
    if response.status != 200:
        raise HomeAssistantError("Closet version is unavailable")
    summary = ClosetSummary.parse(response.json())
    if summary.role != "integration":
        raise HomeAssistantError("Closet version is unavailable")
    return ReleaseVersion.parse(summary.version)


def _app_slug(entry: ConfigEntry) -> str | None:
    slug = entry.data.get(CONF_ADDON_SLUG)
    return slug if isinstance(slug, str) else None


VERSION_REPAIR = VersionRepairSpec(
    DOMAIN,
    "Closet",
    "1449743844",
    ReleaseVersion.parse(LOADED_VERSION),
    _async_probe_version,
    _app_slug,
)


async def async_create_fix_flow(
    hass: HomeAssistant,
    issue_id: str,
    _data: dict[str, str | int | float | None] | None,
) -> RepairsFlow:
    """Resolve the issue against its current loaded configuration entry."""
    return create_version_fix_flow(hass, issue_id, VERSION_REPAIR)
