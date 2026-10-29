-- How many bytes a repository's git data and wiki take on disk, measured
-- after pushes, for storage quotas (`--quota`). 0 until first measured.
ALTER TABLE repositories ADD COLUMN disk_bytes INTEGER NOT NULL DEFAULT 0;
