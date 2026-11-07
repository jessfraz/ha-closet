#!/usr/bin/env python3
"""Exercise the built app, restart, and cold backup restoration on real Docker."""

from __future__ import annotations

import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path


def docker(*args: str, payload: bytes | None = None) -> bytes:
    result = subprocess.run(
        ["docker", *args], input=payload, capture_output=True, check=False
    )
    if result.returncode:
        raise RuntimeError("Docker smoke operation failed")
    return result.stdout


def origin(container: str) -> str:
    binding = docker("port", container, "8080/tcp").decode().strip()
    return "http://" + binding


def wait_ready(base: str) -> None:
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(base + "/health", timeout=2) as response:
                if response.status == 200:
                    return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(0.2)
    raise RuntimeError("Container did not become ready")


def request(
    base: str,
    token: str,
    method: str,
    path: str,
    body: object | None = None,
    revision: str | None = None,
    key: str | None = None,
) -> tuple[int, dict[str, str], object]:
    headers = {"Authorization": "Bearer " + token}
    if body is not None:
        headers["Content-Type"] = "application/json"
    if revision is not None:
        headers["If-Match"] = revision
    if key is not None:
        headers["Idempotency-Key"] = key
    call = urllib.request.Request(
        base + path,
        method=method,
        headers=headers,
        data=json.dumps(body).encode() if body is not None else None,
    )
    with urllib.request.urlopen(call, timeout=10) as response:
        raw = response.read()
        return (
            response.status,
            dict(response.headers.items()),
            json.loads(raw) if raw else None,
        )


def main() -> int:
    image = sys.argv[1] if len(sys.argv) > 1 else "ha-closet:smoke"
    arch = (
        docker("image", "inspect", image, "--format", "{{.Architecture}}")
        .decode()
        .strip()
    )
    if len(sys.argv) > 2 and arch != sys.argv[2]:
        raise RuntimeError("Container architecture does not match the build target")
    suffix = uuid.uuid4().hex[:12]
    app, restored = "closet-smoke-" + suffix, "closet-restored-" + suffix
    data, backup, target = app + "-data", app + "-backup", app + "-restore"
    containers = [app, restored]
    volumes = [data, backup, target]
    catalog_bytes = (
        Path(__file__).resolve().parents[1] / "config/catalog.example.json"
    ).read_bytes()
    catalog = json.loads(catalog_bytes)
    try:
        for volume in volumes:
            docker("volume", "create", volume)
        # App mode generates its own private pairing credentials. The harness
        # captures them only in process memory and never prints their values.
        docker(
            "run",
            "--rm",
            "--volume",
            data + ":/data",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "printf '{}' > /data/options.json",
        )
        docker(
            "run",
            "--detach",
            "--name",
            app,
            "--volume",
            data + ":/data",
            "--publish",
            "127.0.0.1::8080",
            image,
        )
        base = origin(app)
        wait_ready(base)
        credentials = json.loads(docker("exec", app, "cat", "/data/credentials.json"))
        token = credentials["owner"]
        validate = json.loads(
            docker(
                "exec",
                "-i",
                app,
                "ha-closet",
                "inventory",
                "validate",
                "--input",
                "-",
                payload=catalog_bytes,
            )
        )
        assert validate["data"]["count"] == len(catalog)
        _, headers, preview = request(
            base,
            token,
            "POST",
            "/v1/inventory/sync",
            {"items": catalog, "dry_run": True, "allow_empty": False},
        )
        etag = next(value for name, value in headers.items() if name.lower() == "etag")
        payload = {"items": catalog, "dry_run": False, "allow_empty": False}
        status, _, applied = request(
            base,
            token,
            "POST",
            "/v1/inventory/sync",
            payload,
            revision=etag,
            key="smoke-sync-1",
        )
        assert status == 200
        assert applied["created"] == len(catalog)
        assert preview["source_hash"] == applied["source_hash"]
        _, _, listing = request(base, credentials["integration"], "GET", "/v1/items")
        assert listing["total"] == len(catalog)
        docker("stop", "--time", "20", app)
        docker("start", app)
        base = origin(app)
        wait_ready(base)
        assert (
            json.loads(docker("exec", app, "cat", "/data/credentials.json"))
            == credentials
        )
        _, _, replay = request(
            base,
            token,
            "POST",
            "/v1/inventory/sync",
            payload,
            revision=etag,
            key="smoke-sync-1",
        )
        assert replay == applied
        # A cold snapshot includes SQLite, retry receipts and the original
        # pairing identity. Restore into a different volume and instance.
        docker("stop", "--time", "20", app)
        docker(
            "run",
            "--rm",
            "--volume",
            data + ":/from:ro",
            "--volume",
            backup + ":/backup",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "cd /from && tar cf /backup/closet.tar .",
        )
        docker(
            "run",
            "--rm",
            "--volume",
            target + ":/data",
            "--volume",
            backup + ":/backup:ro",
            "--entrypoint",
            "sh",
            image,
            "-c",
            "cd /data && tar xf /backup/closet.tar",
        )
        docker(
            "run",
            "--detach",
            "--name",
            restored,
            "--volume",
            target + ":/data",
            "--publish",
            "127.0.0.1::8080",
            image,
        )
        restored_base = origin(restored)
        wait_ready(restored_base)
        assert (
            json.loads(docker("exec", restored, "cat", "/data/credentials.json"))
            == credentials
        )
        _, _, restored_page = request(restored_base, token, "GET", "/v1/items")
        assert restored_page["total"] == len(catalog)
        _, _, restored_replay = request(
            restored_base,
            token,
            "POST",
            "/v1/inventory/sync",
            payload,
            revision=etag,
            key="smoke-sync-1",
        )
        assert restored_replay == applied
        print(
            "Container passed: architecture, offline CLI, sync, restart, pairing "
            "and cold backup/restore"
        )
        return 0
    finally:
        for container in containers:
            subprocess.run(
                ["docker", "rm", "--force", container], capture_output=True, check=False
            )
        for volume in volumes:
            subprocess.run(
                ["docker", "volume", "rm", volume], capture_output=True, check=False
            )


if __name__ == "__main__":
    raise SystemExit(main())
