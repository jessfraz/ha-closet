CREATE TABLE inventory_fence (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    token_hash TEXT NOT NULL,
    actor TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    pending TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(pending)),
    uploads TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(uploads))
);
