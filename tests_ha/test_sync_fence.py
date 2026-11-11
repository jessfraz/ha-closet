"""Real authenticated HA transport for the durable sync/prune boundary."""

from homeassistant.core import HomeAssistant
from pytest_homeassistant.typing import ClientSessionGenerator

from .conftest import RustBackend, catalog_item
from .test_integration import PREFIX, connect


async def test_fence_routes_are_admin_only_and_forward_only_authenticated_identity(
    hass: HomeAssistant,
    hass_client: ClientSessionGenerator,
    hass_user_token: str,
    rust_backend: RustBackend,
) -> None:
    entry = await connect(hass, rust_backend)
    user = await hass_client(hass_user_token)
    admin = await hass_client()
    token = "a" * 64
    acquire = {"token": token, "expected_revision": 0}
    forged = {"X-Closet-Actor-Id": "ha:forged", "X-Closet-Admin": "true"}
    denied = await user.post(f"{PREFIX}/inventory/fence", json=acquire, headers=forged)
    assert denied.status == 403
    acquired = await admin.post(
        f"{PREFIX}/inventory/fence", json=acquire, headers=forged
    )
    assert acquired.status == 200, await acquired.text()
    assert (await acquired.json())["pending"] == []
    body = {"items": [catalog_item()], "dry_run": False, "allow_empty": False}
    conditions = {"If-Match": '"0"', "Idempotency-Key": "fenced-sync"}
    blocked = await admin.post(
        f"{PREFIX}/inventory/sync", json=body, headers=conditions
    )
    assert blocked.status == 409
    conditions["X-Closet-Sync-Fence"] = token
    applied = await admin.post(
        f"{PREFIX}/inventory/sync", json=body, headers=conditions
    )
    assert applied.status == 200, await applied.text()
    await hass.async_block_till_done()
    assert entry.runtime_data.data.item_count == 1
    for action in ("begin", "verified"):
        claim = await admin.post(
            f"{PREFIX}/inventory/fence/uploads",
            json={"token": token, "sha256": "b" * 64, "action": action},
        )
        assert claim.status == 200, await claim.text()
    prepared = await admin.post(
        f"{PREFIX}/inventory/fence/prepare",
        json={"token": token, "catalog_revision": 1, "images": []},
    )
    assert prepared.status == 200, await prepared.text()
    assert (
        await user.delete(f"{PREFIX}/inventory/fence", json={"token": token})
    ).status == 403
    released = await admin.delete(f"{PREFIX}/inventory/fence", json={"token": token})
    assert released.status == 200, await released.text()
    # Caller-supplied actor headers did not change the actor that owns the nonce.
    conditions["Idempotency-Key"] = "after-release"
    conditions["If-Match"] = '"1"'
    del conditions["X-Closet-Sync-Fence"]
    assert (
        await admin.post(f"{PREFIX}/inventory/sync", json=body, headers=conditions)
    ).status == 200
