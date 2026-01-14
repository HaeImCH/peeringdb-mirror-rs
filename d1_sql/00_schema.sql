
CREATE TABLE IF NOT EXISTS objects (
  resource TEXT NOT NULL,
  obj_id   INTEGER NOT NULL,
  updated  TEXT NOT NULL,
  payload  TEXT NOT NULL,
  PRIMARY KEY (resource, obj_id)
);
CREATE INDEX IF NOT EXISTS objects_resource_updated_idx
  ON objects (resource, updated DESC);
