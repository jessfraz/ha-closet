"""Fictional TLS R700 using the documented HTTP event-stream contract."""

import asyncio
import base64
import json
import ssl
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path

import pytest
from aiohttp import web
from pytest_aiohttp.plugin import AiohttpServer


@dataclass(slots=True)
class FictionalReader:
    """Owned test peer, never a live device or trusted empty-area proof."""

    origin: str = ""
    ca_file: Path = Path()
    connected: bool = True
    stopped: bool = False
    streams: int = 0
    opened: asyncio.Event = field(default_factory=asyncio.Event)
    queue: asyncio.Queue[bytes | None] = field(default_factory=asyncio.Queue)

    async def metadata(self, request: web.Request) -> web.Response:
        """The actual worker discovers these capabilities before streaming."""
        match request.match_info["resource"]:
            case "openapi.json":
                return web.json_response(
                    {
                        "basePath": "/api/v1",
                        "info": {"version": "1.11.0"},
                        "definitions": {
                            "AntennaPort": {"minimum": 1, "maximum": 4},
                            "TransmitPowerCdbm": {"minimum": 1000, "maximum": 3000},
                        },
                        "paths": {"/data/stream": {}},
                    }
                )
            case "status":
                return web.json_response(
                    {
                        "interface": "IoT",
                        "time": datetime.now(UTC).isoformat(),
                        "status": "running",
                    }
                )
            case "http-stream":
                return web.json_response(
                    {"keepAliveIntervalSeconds": 1, "eventBufferSize": 0}
                )
            case "system/image":
                return web.json_response({"primaryFirmware": "10.4.0"})
            case _:
                raise web.HTTPNotFound

    async def stream(self, request: web.Request) -> web.StreamResponse:
        """CRLF keepalives remain transport-only; no fake coverage completion."""
        if not self.connected:
            raise web.HTTPServiceUnavailable
        response = web.StreamResponse(headers={"Content-Type": "application/x-ndjson"})
        await response.prepare(request)
        self.streams += 1
        self.opened.set()
        try:
            while not self.stopped and self.connected:
                try:
                    record = await asyncio.wait_for(self.queue.get(), timeout=0.1)
                except TimeoutError:
                    record = b"\r\n"
                if record is None:
                    break
                await response.write(record)
        except ConnectionError:
            pass
        finally:
            self.streams -= 1
            if not self.streams:
                self.opened.clear()
        return response

    async def emit(self, epcs: tuple[str, ...]) -> dict[str, int]:
        """Multiple timed reports exercise the actual vendor decoder/dwell path."""
        sent: dict[str, int] = {}
        for _ in range(3):
            for epc in epcs:
                now = datetime.now(UTC).isoformat()
                await self.queue.put(
                    (
                        json.dumps(
                            {
                                "timestamp": now,
                                "eventType": "tagInventory",
                                "tagInventoryEvent": {
                                    "epcHex": epc,
                                    "antennaPort": 1,
                                    "lastSeenTime": now,
                                    "peakRssiCdbm": -4100,
                                },
                            }
                        )
                        + "\n"
                    ).encode()
                )
                sent[epc] = sent.get(epc, 0) + 1
            await asyncio.sleep(0.01)
        return sent


@pytest.fixture
async def fictional_reader(
    aiohttp_server: AiohttpServer,
    tmp_path: Path,
    socket_enabled: None,
) -> AsyncIterator[FictionalReader]:
    """Trust only the existing fictional test CA in this subprocess."""
    certificates = Path(__file__).parents[1] / "tests/fixtures/tls"

    def pem(name: str, label: str) -> Path:
        data = (certificates / f"{name}.der").read_bytes()
        encoded = base64.encodebytes(data).decode()
        target = tmp_path / f"{name}.pem"
        target.write_text(f"-----BEGIN {label}-----\n{encoded}-----END {label}-----\n")
        return target

    ca = await asyncio.to_thread(pem, "ca", "CERTIFICATE")
    certificate = await asyncio.to_thread(pem, "cert", "CERTIFICATE")
    key = await asyncio.to_thread(pem, "key", "PRIVATE KEY")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, key)
    peer = FictionalReader(ca_file=ca)
    app = web.Application()
    app.router.add_get("/api/v1/data/stream", peer.stream)
    app.router.add_get("/api/v1/{resource:.*}", peer.metadata)
    server = await aiohttp_server(app, ssl=context)
    peer.origin = str(server.make_url("/")).rstrip("/")
    try:
        yield peer
    finally:
        peer.stopped = True
        await peer.queue.put(None)
