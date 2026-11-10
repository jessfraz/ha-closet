"""Native version repair for the Closet app and companion."""

from typing import TYPE_CHECKING

from homeassistant.components.repairs import RepairsFlow
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant

from ._app_kit.repairs import VersionRepairSpec, create_version_fix_flow
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .const import CONF_ADDON_SLUG, DOMAIN

if TYPE_CHECKING:
    from .coordinator import ClosetCoordinator


async def _async_probe_version(
    _hass: HomeAssistant, entry: ConfigEntry
) -> ReleaseVersion:
    runtime: ClosetCoordinator = entry.runtime_data
    return ReleaseVersion.parse((await runtime.async_read_system()).version)


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
