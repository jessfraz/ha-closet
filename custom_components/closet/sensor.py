"""Aggregate catalog sensors, not an entity per garment."""

from homeassistant.components.sensor import SensorEntity, SensorEntityDescription
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import ClosetConfigEntry
from .const import DOMAIN
from .coordinator import ClosetCoordinator

DESCRIPTIONS = (
    SensorEntityDescription(
        key="item_count", translation_key="items", icon="mdi:hanger"
    ),
    SensorEntityDescription(
        key="archived_count",
        translation_key="archived",
        icon="mdi:archive-outline",
        entity_registry_enabled_default=False,
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ClosetConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Expose only aggregate counts, preserving stable entry-owned IDs."""
    async_add_entities(
        ClosetSensor(entry.runtime_data, entry.entry_id, description)
        for description in DESCRIPTIONS
    )


class ClosetSensor(CoordinatorEntity[ClosetCoordinator], SensorEntity):
    """One count from a completely validated summary."""

    _attr_has_entity_name = True

    def __init__(
        self,
        coordinator: ClosetCoordinator,
        entry_id: str,
        description: SensorEntityDescription,
    ) -> None:
        super().__init__(coordinator)
        self.entity_description = description
        self._attr_unique_id = f"{entry_id}_{description.key}"
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, entry_id)}, name="Closet", manufacturer="Closet"
        )
        self._set_value()

    def _set_value(self) -> None:
        """No garment names, IDs or source metadata enter recorder attributes."""
        data = self.coordinator.data
        self._attr_native_value = {
            "item_count": data.item_count,
            "archived_count": data.archived_count,
        }[self.entity_description.key]

    @callback
    def _handle_coordinator_update(self) -> None:
        self._set_value()
        super()._handle_coordinator_update()
