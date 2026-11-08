DROP INDEX items_active_confirmed;
ALTER TABLE items DROP COLUMN confirmed;

-- Store::open completes the typed JSON and hash rewrite before serving requests.
-- The marker survives an interrupted startup; that rewrite commits atomically
-- with its removal, so a restart can safely finish it exactly once.
CREATE TABLE ownership_removal_pending (singleton INTEGER PRIMARY KEY);
INSERT INTO ownership_removal_pending VALUES (1);
