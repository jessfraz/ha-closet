"""Allowlisted tracking DTOs; Rust owns every readiness and evidence decision."""

from dataclasses import dataclass
from enum import StrEnum
from uuid import UUID


class UnitStatus(StrEnum):
    """One user-facing status, never separate care/storage state machines."""

    UNKNOWN = "unknown"
    CLEAN_READY = "clean_ready"
    WORN_READY = "worn_ready"
    NEEDS_LAUNDRY = "needs_laundry"
    QUEUED_DRY_CLEANING = "queued_dry_cleaning"
    BEING_WORN = "being_worn"
    IN_WASH = "in_wash"
    AT_CLEANERS = "at_cleaners"


def count(value: object) -> int:
    """Booleans and missing values are not measured zeroes."""
    if type(value) is not int or value < 0:
        raise ValueError("Invalid tracking count")
    return value


def flag(value: object) -> bool:
    """Reject malformed flags instead of turning strings into true."""
    if type(value) is not bool:
        raise ValueError("Invalid tracking flag")
    return value


@dataclass(frozen=True, slots=True)
class TrackedUnit:
    """Only fields used by optional status entities, not radio identifiers."""

    id: str
    ordinal: int
    status: UnitStatus
    ready: bool
    paused: bool
    retired: bool
    archived: bool
    evidence: str

    @classmethod
    def parse(cls, value: object, ready: frozenset[str]) -> TrackedUnit:
        """Validate a unit without retaining EPCs, catalog IDs or private notes."""
        if not isinstance(value, dict):
            raise ValueError("Invalid tracking unit")
        unit_id = value.get("id")
        status = value.get("status")
        evidence = value.get("evidence")
        if (
            not isinstance(unit_id, str)
            or str(UUID(unit_id)) != unit_id
            or not isinstance(status, str)
            or evidence not in {"unknown", "manual", "rfid"}
        ):
            raise ValueError("Invalid tracking identity or evidence")
        ordinal = count(value.get("ordinal"))
        if ordinal == 0:
            raise ValueError("Invalid tracking ordinal")
        return cls(
            unit_id,
            ordinal,
            UnitStatus(status),
            unit_id in ready,
            flag(value.get("paused")),
            flag(value.get("retired")),
            flag(value.get("archived")),
            str(evidence),
        )


COUNT_KEYS = (
    "certified_ready",
    "total_units",
    "tagged_units",
    *UnitStatus,
    "exceptions",
    "retired",
    "archived",
)


@dataclass(frozen=True, slots=True)
class TrackingSummary:
    """Bounded operational counts plus optional per-unit status."""

    revision: int
    enabled: bool
    counts: dict[str, int]
    coverage_healthy: bool
    warming_up: bool
    required_ports: int
    reader_count: int
    port_count: int
    units: tuple[TrackedUnit, ...]

    @classmethod
    def parse(cls, value: object) -> TrackingSummary:
        """Read a complete snapshot; never reinterpret missing data as healthy."""
        if not isinstance(value, dict) or value.get("schema_version") != 1:
            raise ValueError("Unsupported tracking snapshot")
        config = value.get("config")
        health = value.get("health")
        counts = value.get("counts")
        units = value.get("units")
        ready = value.get("certified_ready_unit_ids")
        if (
            not isinstance(config, dict)
            or not isinstance(health, dict)
            or not isinstance(counts, dict)
            or not isinstance(units, list)
            or not isinstance(ready, list)
        ):
            raise ValueError("Invalid tracking snapshot")
        ready_ids: set[str] = set()
        for unit_id in ready:
            if not isinstance(unit_id, str) or str(UUID(unit_id)) != unit_id:
                raise ValueError("Invalid readiness identity")
            ready_ids.add(unit_id)
        certified = frozenset(ready_ids)
        parsed = tuple(TrackedUnit.parse(unit, certified) for unit in units)
        ids = {unit.id for unit in parsed}
        if len(ids) != len(parsed) or not ready_ids <= ids:
            raise ValueError("Inconsistent tracking identities")
        readers = config.get("readers")
        ports = config.get("ports")
        if not isinstance(readers, list) or not isinstance(ports, list):
            raise ValueError("Invalid tracking topology")
        return cls(
            count(value.get("tracking_revision")),
            flag(config.get("enabled")),
            {key: count(counts.get(key)) for key in COUNT_KEYS},
            flag(health.get("absence_coverage_healthy")),
            flag(health.get("warming_up")),
            count(health.get("required_ports")),
            len(readers),
            len(ports),
            parsed,
        )
