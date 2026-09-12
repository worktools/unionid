ALTER TABLE tasks ADD COLUMN state_archived_at_micros INTEGER;
ALTER TABLE tasks ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
INSERT INTO state_variants(tag) VALUES ('Archived');
