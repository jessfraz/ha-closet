-- Installation progress is part of the typed unit body, not a query dimension.
ALTER TABLE tracking_units DROP COLUMN installation_state;

-- Global observability is separate from garment state and raw RF diagnostics.
CREATE TABLE tracking_coverage (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    verified_through TEXT NOT NULL,
    ended_at TEXT,
    end_reason TEXT,
    configuration_version INTEGER NOT NULL REFERENCES tracking_configurations(version),
    CHECK (verified_through >= started_at),
    CHECK ((ended_at IS NULL AND end_reason IS NULL)
        OR (ended_at IS NOT NULL AND end_reason IS NOT NULL AND ended_at = verified_through))
);
CREATE UNIQUE INDEX tracking_coverage_open ON tracking_coverage((1)) WHERE ended_at IS NULL;
