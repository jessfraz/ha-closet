"""Cached allowlisted diagnostics without catalog, prompts or credentials."""

from homeassistant.core import HomeAssistant

from . import ClosetConfigEntry


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: ClosetConfigEntry
) -> dict[str, object]:
    """A diagnostic download never triggers network calls or reads secrets."""
    coordinator = entry.runtime_data
    summary = coordinator.data
    return {
        "integration_version": coordinator.integration_version,
        "available": coordinator.last_update_success,
        "backend_version": summary.version if summary else None,
        "item_count": summary.item_count if summary else None,
        "archived_count": summary.archived_count if summary else None,
        "today_configured": summary.today_configured if summary else None,
    }
