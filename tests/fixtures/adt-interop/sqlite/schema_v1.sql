CREATE TABLE state_variants (tag TEXT PRIMARY KEY);
INSERT INTO state_variants(tag) VALUES ('Queued'),('Running'),('Failed'),('Done');

CREATE TABLE tasks (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  state_tag TEXT NOT NULL REFERENCES state_variants(tag),
  state_attempt INTEGER,
  state_message TEXT,
  state_retry_at_micros INTEGER,
  note_outer_some INTEGER NOT NULL CHECK (note_outer_some IN (0,1)),
  note_value TEXT,
  price_cents INTEGER NOT NULL,
  created_at_micros INTEGER NOT NULL,
  CHECK ((state_tag = 'Running') = (state_attempt IS NOT NULL)),
  CHECK ((state_tag = 'Failed') = (state_message IS NOT NULL)),
  CHECK (state_retry_at_micros IS NULL OR state_tag = 'Failed'),
  CHECK (note_outer_some = 1 OR note_value IS NULL)
);

CREATE INDEX tasks_state ON tasks(state_tag);
