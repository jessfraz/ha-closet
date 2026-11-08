"""Validated aggregate data; garment records stay in the app."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class ClosetSummary:
    """Only aggregate health and counts are published as entity state."""

    version: str
    role: str
    catalog_revision: int
    item_count: int
    archived_count: int
    today_configured: bool

    @classmethod
    def parse(cls, value: object) -> ClosetSummary:
        """Reject partial/invalid results rather than interpreting them as zero."""
        if not isinstance(value, dict):
            raise ValueError("Invalid Closet summary")
        version = value.get("version")
        role = value.get("role")
        if (
            not isinstance(version, str)
            or not version
            or len(version) > 64
            or not isinstance(role, str)
            or role not in {"owner", "integration"}
        ):
            raise ValueError("Invalid Closet version or credential role")
        counts: list[int] = []
        for key in (
            "catalog_revision",
            "item_count",
            "archived_count",
        ):
            count = value.get(key)
            if type(count) is not int or count < 0:
                raise ValueError("Invalid Closet count")
            counts.append(count)
        configured = value.get("today_configured")
        if type(configured) is not bool:
            raise ValueError("Invalid Closet capability")
        return cls(version, role, *counts, configured)
