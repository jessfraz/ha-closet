-- RFID is a runtime projection over catalog IDs, not another garment catalog.
CREATE TABLE tracking_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    configuration_version INTEGER NOT NULL DEFAULT 0 CHECK (configuration_version >= 0),
    enabled INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0,1)),
    config TEXT NOT NULL CHECK (json_valid(config))
);
INSERT INTO tracking_state(singleton,config) VALUES(1,'{"schema_version":1,"enabled":false,"areas":[],"readers":[],"ports":[],"thresholds":{"positive_dwell_ms":1500,"positive_min_reads":3,"movement_hysteresis_ms":2500,"absence_ms":120000,"warmup_ms":30000,"coverage_max_gap_ms":5000,"candidate_fresh_ms":5000,"reader_event_max_age_ms":60000}}');
CREATE TABLE tracking_inventory (
    catalog_id TEXT PRIMARY KEY REFERENCES items(id),
    catalog_quantity INTEGER CHECK (catalog_quantity IS NULL OR catalog_quantity > 0),
    declared_quantity INTEGER CHECK (declared_quantity IS NULL OR declared_quantity > 0),
    next_ordinal INTEGER NOT NULL CHECK (next_ordinal > 0),
    needs_reconciliation INTEGER NOT NULL DEFAULT 0 CHECK (needs_reconciliation IN (0,1)),
    capacity_limited INTEGER NOT NULL DEFAULT 0 CHECK (capacity_limited IN (0,1)),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0,1))
);
CREATE TABLE tracking_units (
    id TEXT PRIMARY KEY,
    catalog_id TEXT NOT NULL REFERENCES tracking_inventory(catalog_id),
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    epc TEXT,
    status TEXT NOT NULL CHECK (status IN ('unknown','clean_ready','worn_ready','needs_laundry','queued_dry_cleaning','being_worn','in_wash','at_cleaners')),
    last_confirmed_area TEXT,
    current_area TEXT,
    baseline_at TEXT,
    installation_state TEXT NOT NULL CHECK (installation_state IN ('needs_tag','seamstress_queued','installed','exempt')),
    evidence TEXT NOT NULL CHECK (evidence IN ('unknown','manual','rfid')),
    fence_at TEXT NOT NULL,
    paused INTEGER NOT NULL CHECK (paused IN (0,1)),
    retired INTEGER NOT NULL CHECK (retired IN (0,1)),
    archived INTEGER NOT NULL CHECK (archived IN (0,1)),
    exception TEXT,
    changed_at TEXT NOT NULL,
    last_event_id INTEGER,
    body TEXT NOT NULL CHECK (json_valid(body)),
    UNIQUE(catalog_id,ordinal)
);
CREATE UNIQUE INDEX tracking_active_epc ON tracking_units(epc) WHERE epc IS NOT NULL AND retired=0;
CREATE INDEX tracking_units_catalog_status ON tracking_units(catalog_id,retired,archived,status);
CREATE TABLE tracking_sessions (
    id TEXT PRIMARY KEY,
    port_id TEXT NOT NULL,
    state TEXT NOT NULL,
    capture_version INTEGER NOT NULL,
    body TEXT NOT NULL CHECK (json_valid(body))
);
CREATE UNIQUE INDEX tracking_active_station ON tracking_sessions(port_id) WHERE state IN ('active','recovery_required');
CREATE TABLE tracking_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_id TEXT NOT NULL REFERENCES tracking_units(id),
    at TEXT NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL CHECK (json_valid(body))
);
CREATE INDEX tracking_history_unit ON tracking_history(unit_id,id DESC);

CREATE TABLE tracking_bindings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unit_id TEXT NOT NULL REFERENCES tracking_units(id),
    epc TEXT NOT NULL,
    bound_at TEXT NOT NULL,
    retired_at TEXT,
    retirement TEXT
);
CREATE UNIQUE INDEX tracking_binding_epc ON tracking_bindings(epc) WHERE retired_at IS NULL;
CREATE UNIQUE INDEX tracking_binding_unit ON tracking_bindings(unit_id) WHERE retired_at IS NULL;
CREATE INDEX tracking_bindings_history ON tracking_bindings(unit_id,id);
CREATE TABLE tracking_reader_operations (
    reader_id TEXT PRIMARY KEY,
    body TEXT NOT NULL CHECK(json_valid(body))
);

CREATE TABLE tracking_reader_controls (
    reader_id TEXT PRIMARY KEY,
    body TEXT NOT NULL CHECK(json_valid(body))
);

CREATE TABLE tracking_configurations (
    version INTEGER PRIMARY KEY,
    at TEXT NOT NULL,
    body TEXT NOT NULL CHECK(json_valid(body))
);
INSERT INTO tracking_configurations(version,at,body) SELECT configuration_version,'1970-01-01T00:00:00+00:00',config FROM tracking_state;
