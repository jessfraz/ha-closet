"""Poll the service's aggregate state without storing a second inventory."""

import logging
from contextlib import suppress
from dataclasses import replace
from datetime import timedelta
from urllib.parse import urlencode

from aiohttp import ClientError
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import ConfigEntryAuthFailed
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from ._app_kit.repairs import VersionRepairs
from ._app_kit.version import LOADED_VERSION
from ._app_kit.versions import ReleaseVersion
from .client import ClosetClient, ResponseTooLarge
from .const import DOMAIN
from .models import ClosetSummary
from .tracking import TrackedUnit, TrackingSummary

_LOGGER = logging.getLogger(__name__)


class ProjectionInvalidated(Exception):
    """A revision-bound page changed; discard every accumulated unit."""


class ClosetCoordinator(DataUpdateCoordinator[ClosetSummary]):
    """One shared session and one entry-owned polling lifecycle."""

    def __init__(
        self,
        hass: HomeAssistant,
        entry: ConfigEntry,
        client: ClosetClient,
    ) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            config_entry=entry,
            update_interval=timedelta(seconds=60),
        )
        self.client = client
        self.integration_version = LOADED_VERSION
        self.version_repairs: VersionRepairs | None = None

    async def async_read_system(self) -> ClosetSummary:
        response = await self.client.request("GET", "/v1/system")
        if response.status in {401, 403}:
            raise ConfigEntryAuthFailed("The Closet credential was rejected")
        if response.status != 200:
            raise UpdateFailed("Closet is unavailable")
        summary = ClosetSummary.parse(response.json())
        if summary.role != "integration":
            raise ConfigEntryAuthFailed("Use Closet's integration credential")
        return summary

    async def _async_update_data(self) -> ClosetSummary:
        try:
            summary = await self.async_read_system()
            summary = replace(summary, tracking=await self._tracking_projection())
        except (ClientError, TimeoutError, ResponseTooLarge, ValueError) as err:
            raise UpdateFailed("Cannot read Closet") from err
        if self.version_repairs is not None:
            with suppress(ValueError):
                self.version_repairs.async_report(ReleaseVersion.parse(summary.version))
        return summary

    async def _tracking_projection(self) -> TrackingSummary:
        """Publish complete reads, restarting at most once after a concurrent write."""
        for attempt in range(2):
            try:
                return await self._projection_pages()
            except ProjectionInvalidated:
                if attempt == 1:
                    raise UpdateFailed("Closet tracking changed while paging") from None
        raise UpdateFailed("Cannot read Closet tracking")

    async def _projection_pages(self) -> TrackingSummary:
        first: TrackingSummary | None = None
        units: list[TrackedUnit] = []
        identities: set[str] = set()
        cursors: set[str] = set()
        cursor: str | None = None
        while True:
            query = {"limit": "500"}
            if cursor is not None:
                query["page_token"] = cursor
            response = await self.client.request(
                "GET", f"/v1/tracking/projection?{urlencode(query)}"
            )
            if response.status in {401, 403}:
                raise ConfigEntryAuthFailed("The Closet credential was rejected")
            if (
                response.status == 409
                and response.json().get("error_code") == "page_invalidated"
            ):
                raise ProjectionInvalidated
            if response.status != 200:
                raise UpdateFailed("Closet tracking is unavailable")
            document = response.json()
            page = TrackingSummary.parse(document)
            if first is None:
                first = page
            elif page.revision != first.revision:
                raise ProjectionInvalidated
            for unit in page.units:
                if unit.id in identities:
                    raise ValueError("Repeated tracking unit")
                identities.add(unit.id)
                units.append(unit)
            next_page = document.get("next_page")
            if next_page is None:
                return replace(first, units=tuple(units))
            if (
                not isinstance(next_page, str)
                or not next_page
                or len(next_page) > 512
                or next_page in cursors
            ):
                raise ValueError("Invalid tracking cursor")
            cursors.add(next_page)
            cursor = next_page
