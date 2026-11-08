"""Bounded counts and optional stable physical-piece status entities."""

import asyncio

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorEntityDescription,
)
from homeassistant.core import HomeAssistant, callback
from homeassistant.helpers.entity import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from . import ClosetConfigEntry
from .const import DOMAIN
from .coordinator import ClosetCoordinator
from .tracking import COUNT_KEYS, TrackedUnit, UnitStatus

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
    *(
        SensorEntityDescription(
            key=f"tracking_{key}",
            translation_key=f"tracking_{key}",
            icon="mdi:hanger",
            entity_registry_enabled_default=key == "certified_ready",
        )
        for key in COUNT_KEYS
    ),
    SensorEntityDescription(
        key="tracking_health",
        translation_key="tracking_health",
        icon="mdi:access-point",
        device_class=SensorDeviceClass.ENUM,
        options=["disabled", "warming_up", "healthy", "needs_attention"],
    ),
)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ClosetConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Reconcile optional piece entities without duplicating Rust's tracking rules."""
    coordinator = entry.runtime_data
    async_add_entities(
        ClosetSensor(coordinator, entry.entry_id, description)
        for description in DESCRIPTIONS
    )
    entities: dict[str, ClosetUnitSensor] = {}
    lock = asyncio.Lock()

    async def reconcile() -> None:
        # Refresh/reload and archive/restore can overlap. Serialize the owned
        # entity lifetimes, then read the newest snapshot rather than a stale one.
        async with lock:
            data = coordinator.data.tracking
            current = (
                {
                    unit.id: unit
                    for unit in data.units
                    if not unit.retired and not unit.archived
                }
                if data
                else {}
            )
            for unit_id in entities.keys() - current.keys():
                entity = entities.pop(unit_id)
                if entity.hass is not None:
                    await entity.async_remove()
            added: list[ClosetUnitSensor] = []
            for unit_id, unit in current.items():
                if unit_id not in entities:
                    entity = ClosetUnitSensor(coordinator, entry.entry_id, unit)
                    entities[unit_id] = entity
                    added.append(entity)
            if added:
                async_add_entities(added)

    @callback
    def updated() -> None:
        entry.async_create_background_task(hass, reconcile(), "Reconcile Closet pieces")

    entry.async_on_unload(coordinator.async_add_listener(updated))
    await reconcile()


class ClosetSensor(CoordinatorEntity[ClosetCoordinator], SensorEntity):
    """Connectivity controls availability, not whether clothing is ready."""

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
        """No garment names, identifiers, RF evidence or source metadata attributes."""
        data = self.coordinator.data
        key = self.entity_description.key
        if key == "item_count":
            self._attr_native_value = data.item_count
        elif key == "archived_count":
            self._attr_native_value = data.archived_count
        elif key == "tracking_health":
            tracking = data.tracking
            self._attr_native_value = (
                "disabled"
                if not tracking or not tracking.enabled
                else "warming_up"
                if tracking.warming_up
                else "healthy"
                if tracking.coverage_healthy
                else "needs_attention"
            )
        else:
            self._attr_native_value = (
                data.tracking.counts[key.removeprefix("tracking_")]
                if data.tracking
                else None
            )

    @callback
    def _handle_coordinator_update(self) -> None:
        self._set_value()
        super()._handle_coordinator_update()


class ClosetUnitSensor(CoordinatorEntity[ClosetCoordinator], SensorEntity):
    """Opt-in status per stable physical unit, not per EPC or purchase ordinal."""

    _attr_has_entity_name = True
    _attr_entity_registry_enabled_default = False
    _attr_device_class = SensorDeviceClass.ENUM
    _attr_options = [str(status) for status in UnitStatus]
    _attr_icon = "mdi:hanger"
    _attr_translation_key = "unit_status"

    def __init__(
        self,
        coordinator: ClosetCoordinator,
        entry_id: str,
        unit: TrackedUnit,
    ) -> None:
        super().__init__(coordinator)
        self.unit_id = unit.id
        self._attr_unique_id = f"{entry_id}_unit_{unit.id}"
        self._attr_name = f"Piece {unit.id[-8:]}"
        self._attr_device_info = DeviceInfo(identifiers={(DOMAIN, entry_id)})
        self._set_unit(unit)

    def _set_unit(self, unit: TrackedUnit) -> None:
        self._attr_native_value = str(unit.status)
        self._attr_extra_state_attributes = {
            "ready_to_wear": unit.ready,
            "evidence": unit.evidence,
            "automation_paused": unit.paused,
        }

    @callback
    def _handle_coordinator_update(self) -> None:
        tracking = self.coordinator.data.tracking
        unit = (
            next((unit for unit in tracking.units if unit.id == self.unit_id), None)
            if tracking
            else None
        )
        if unit is not None:
            self._set_unit(unit)
        super()._handle_coordinator_update()
