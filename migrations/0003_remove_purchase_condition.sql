-- No separate condition column exists. Store::open rewrites catalog JSON and
-- recoverable sync digests atomically, then removes this restart-safe marker.
CREATE TABLE condition_removal_pending (singleton INTEGER PRIMARY KEY);
INSERT INTO condition_removal_pending VALUES (1);
