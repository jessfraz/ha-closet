"""Fictional reader evidence through the actual protected tracking API."""

import asyncio
from datetime import datetime
from uuid import uuid4

from aiohttp import ClientSession
from homeassistant.util.json import JsonObjectType

from .conftest import RustBackend


async def snapshot(backend: RustBackend) -> JsonObjectType:
    """Read the complete owner snapshot."""
    async with ClientSession() as client:
        async with client.get(
            f"{backend.origin}/v1/tracking",
            headers={"Authorization": f"Bearer {backend.owner_token}"},
        ) as response:
            assert response.status == 200, await response.text()
            return await response.json()


async def command(backend: RustBackend, body: JsonObjectType) -> JsonObjectType:
    """Exercise real revision and idempotency preconditions."""
    current = await snapshot(backend)
    async with ClientSession() as client:
        async with client.post(
            f"{backend.origin}/v1/tracking/commands",
            headers={
                "Authorization": f"Bearer {backend.owner_token}",
                "If-Match": f'"{current["tracking_revision"]}"',
                "Idempotency-Key": str(uuid4()),
            },
            json=body,
        ) as response:
            assert response.status == 200, await response.text()
            return await response.json()


async def configure(backend: RustBackend) -> None:
    """One real area and one isolated enrollment port, no physical RF claim."""
    assert backend.reader is not None
    current = await snapshot(backend)
    config = current["config"]
    assert isinstance(config, dict)
    config.update(
        enabled=True,
        areas=[
            {
                "id": "example-closet",
                "label": "Example closet",
                "kind": "closet",
                "enabled": True,
            },
            {
                "id": "example-hamper",
                "label": "Example hamper",
                "kind": "hamper",
                "enabled": True,
            },
        ],
        readers=[
            {
                "id": "example-reader",
                "label": "Example reader",
                "enabled": True,
                "connection": {
                    "type": "impinj_iot",
                    "base_url": backend.reader.origin,
                    "credentials": {"type": "none"},
                },
            }
        ],
        ports=[
            {
                "id": "example-enroll",
                "reader_id": "example-reader",
                "antenna": 1,
                "area_id": None,
                "role": "enrollment",
                "enabled": True,
                "required_for_absence": False,
                "commissioned": False,
                "transmit_power_cdbm": None,
                "dwell_ms": None,
            }
        ],
    )
    thresholds = config["thresholds"]
    assert isinstance(thresholds, dict)
    thresholds.update(
        positive_dwell_ms=1, positive_min_reads=1, candidate_fresh_ms=1000
    )
    await command(backend, {"type": "configure", "config": config})


def units(data: JsonObjectType) -> list[JsonObjectType]:
    """Narrow the actual parsed wire array for consumers."""
    result = data["units"]
    assert isinstance(result, list)
    narrowed: list[JsonObjectType] = []
    for unit in result:
        assert isinstance(unit, dict)
        narrowed.append(unit)
    return narrowed


def capture_reads(
    data: JsonObjectType, epcs: tuple[str, ...]
) -> dict[str, dict[str, datetime]]:
    """Actual accepted enrollment reports, keyed by unique reader event reference."""
    diagnostics = data["diagnostics"]
    assert isinstance(diagnostics, dict)
    recent = diagnostics["recent_reads"]
    assert isinstance(recent, list)
    result: dict[str, dict[str, datetime]] = {epc: {} for epc in epcs}
    for read in recent:
        assert isinstance(read, dict)
        epc, reference, observed = read["epc"], read["event_ref"], read["observed_at"]
        if (
            isinstance(epc, str)
            and epc in result
            and read["purpose"] == "enrollment"
            and isinstance(reference, str)
            and isinstance(observed, str)
        ):
            result[epc][reference] = datetime.fromisoformat(observed)
    return result


class ReaderEvidence:
    """Drive the actual configured TLS reader rather than spoofing its session."""

    def __init__(self, backend: RustBackend) -> None:
        assert backend.reader is not None
        self.backend = backend
        self.reader = backend.reader

    async def observe(
        self,
        epcs: tuple[str, ...] = (),
        capture_session: str | None = None,
        *,
        connected: bool = True,
    ) -> None:
        """Use the native reader worker and await the actual capture/health result."""
        self.reader.connected = connected
        if not connected:
            await self.reader.queue.put(None)
        async with asyncio.timeout(15):
            if connected:
                await self.reader.opened.wait()
                # The stream opens just before the worker publishes connection state.
                await asyncio.sleep(0.05)
                before = await snapshot(self.backend)
                previous = capture_reads(before, epcs)
                sessions = before["sessions"]
                assert isinstance(sessions, list)
                capture = next(
                    (
                        session
                        for session in sessions
                        if isinstance(session, dict)
                        and session["id"] == capture_session
                    ),
                    None,
                )
                if epcs:
                    assert isinstance(capture, dict)
                delivered = await self.reader.emit(epcs)
            while True:
                data = await snapshot(self.backend)
                health = data["health"]
                assert isinstance(health, dict)
                ports = health["ports"]
                assert isinstance(ports, list)
                disconnected = any(
                    isinstance(port, dict) and port["state"] == "disconnected"
                    for port in ports
                )
                sessions = data["sessions"]
                assert isinstance(sessions, list)
                if not connected and disconnected and not self.reader.streams:
                    return
                if connected and not disconnected:
                    if not epcs:
                        return
                    session = next(
                        (
                            entry
                            for entry in sessions
                            if isinstance(entry, dict)
                            and entry["id"] == capture_session
                        ),
                        None,
                    )
                    if isinstance(session, dict):
                        assert isinstance(capture, dict)
                        assert session["state"] == "active"
                        assert session["cursor"] == capture["cursor"]
                        assert (
                            session["capture_started_at"]
                            == capture["capture_started_at"]
                        )
                        started = session["capture_started_at"]
                        assert isinstance(started, str)
                        capture_started = datetime.fromisoformat(started)
                        current = capture_reads(data, epcs)
                        # Both diagnostics and candidates use server-domain time.
                        # Await every newly emitted report, not a raw reader UTC
                        # deadline or an already-present candidate from an old scan.
                        fresh = {
                            epc: [
                                observed
                                for reference, observed in current[epc].items()
                                if reference not in previous[epc]
                                and observed > capture_started
                            ]
                            for epc in epcs
                        }
                        if not all(
                            len(fresh[epc]) >= count for epc, count in delivered.items()
                        ):
                            await asyncio.sleep(0.05)
                            continue
                        candidates = session["candidates"]
                        assert isinstance(candidates, list)
                        observed = {
                            candidate["epc"]
                            for candidate in candidates
                            if isinstance(candidate, dict)
                            and isinstance(candidate["epc"], str)
                        }
                        if set(epcs) <= observed and all(
                            isinstance(candidate, dict)
                            and isinstance(candidate["epc"], str)
                            and isinstance(candidate["last_seen_at"], str)
                            and datetime.fromisoformat(candidate["last_seen_at"])
                            >= max(fresh[candidate["epc"]])
                            for candidate in candidates
                            if isinstance(candidate, dict)
                            and candidate.get("epc") in delivered
                        ):
                            return
                await asyncio.sleep(0.05)
