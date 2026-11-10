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


NATIVE_RESTORE = b"""
import io
from pathlib import Path
import shutil
import tarfile
import tempfile
from securetar import SecureTarArchive, SecureTarFile, atomic_contents_add

password = "container-native-restore-only"
encrypted = io.BytesIO()
with SecureTarArchive(
    fileobj=encrypted, mode="w", password=password, create_version=3
) as archive:
    with archive.create_tar("./app.tar.gz") as private:
        atomic_contents_add(
            private, Path("/source"), file_filter=lambda _: False, arcname="data"
        )
encrypted.seek(0)
with (
    tempfile.TemporaryDirectory() as extracted,
    tarfile.open(fileobj=encrypted, mode="r:") as outer,
):
    member = next(item for item in outer if item.name.endswith("app.tar.gz"))
    with SecureTarFile(
        fileobj=outer.extractfile(member), password=password, gzip=True
    ) as private:
        private.extractall(path=extracted, filter="tar")
    shutil.copytree(
        Path(extracted, "data"), "/restored", symlinks=True, dirs_exist_ok=True
    )
for name in ("credentials.json", "closet.sqlite3", "owned-nested/marker"):
    metadata = Path("/restored", name).stat()
    assert metadata.st_uid == 0 and metadata.st_mode & 0o777 == 0o644
"""


def restore_native(arch: str, source: str, target: str) -> None:
    images = {
        "amd64": (
            "ghcr.io/home-assistant/amd64-hassio-supervisor@sha256:"
            "f5f3728cf7a9a637485576c7588d209a1527fe9d6bb46f7326ef9aa629c55713"
        ),
        "arm64": (
            "ghcr.io/home-assistant/aarch64-hassio-supervisor@sha256:"
            "0f606324063cd5e7e591fb71efb5214ff56a05514898e33e461757950abc2914"
        ),
    }
    docker(
        "run",
        "--rm",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "FOWNER",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,noexec",
        "--mount",
        f"type=volume,source={source},target=/source,readonly",
        "--mount",
        f"type=volume,source={target},target=/restored",
        "--entrypoint",
        "python3",
        "--interactive",
        images[arch],
        "-",
        payload=NATIVE_RESTORE,
    )
    print(
        "Native encrypted restore recreated root-owned0644 private files.", flush=True
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
    data, target = app + "-data", app + "-restore"
    containers = [app, restored]
    volumes = [data, target]
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
            "--cap-drop",
            "FOWNER",
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
            "--network",
            "none",
            "--volume",
            data + ":/data",
            "--entrypoint",
            "sh",
            image,
            "-ec",
            "umask 077; mkdir /data/owned-nested; "
            'printf "fictional-private-state" > /data/owned-nested/marker; '
            "chown -R 10001:10001 /data/owned-nested",
        )
        restore_native(arch, data, target)
        docker(
            "run",
            "--detach",
            "--name",
            restored,
            "--cap-drop",
            "FOWNER",
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
        assert (
            docker(
                "exec",
                "--user",
                "10001:10001",
                restored,
                "cat",
                "/data/owned-nested/marker",
            )
            == b"fictional-private-state"
        )
        for path in (
            "/data/credentials.json",
            "/data/closet.sqlite3",
            "/data/owned-nested/marker",
        ):
            assert (
                docker("exec", restored, "stat", "-c", "%a:%u:%g", path).strip()
                == b"600:10001:10001"
            )
        assert (
            docker("exec", restored, "stat", "-c", "%a:%u:%g", "/data").strip()
            == b"700:10001:10001"
        )
        subprocess.run(
            [str(Path(__file__).with_name("private-data-smoke.sh")), image], check=True
        )
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
