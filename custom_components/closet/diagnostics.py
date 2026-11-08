"""Cached allowlisted diagnostics without catalog, prompts or credentials."""

from homeassistant.core import HomeAssistant

from . import ClosetConfigEntry


async def async_get_config_entry_diagnostics(
    hass: HomeAssistant, entry: ClosetConfigEntry
) -> dict[str, object]:
    """A diagnostic download never triggers network calls or reads secrets."""
    coordinator = entry.runtime_data
    summary = coordinator.data
    tracking = summary.tracking if summary else None
    return {
        "integration_version": coordinator.integration_version,
        "available": coordinator.last_update_success,
        "backend_version": summary.version if summary else None,
        "item_count": summary.item_count if summary else None,
        "archived_count": summary.archived_count if summary else None,
        "today_configured": summary.today_configured if summary else None,
        "tracking": {
            "enabled": tracking.enabled,
            "counts": dict(tracking.counts),
            "coverage_healthy": tracking.coverage_healthy,
            "warming_up": tracking.warming_up,
            "reader_count": tracking.reader_count,
            "port_count": tracking.port_count,
        }
        if tracking
        else None,
    }
