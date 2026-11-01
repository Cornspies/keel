-- What people say about themselves on their profile.
ALTER TABLE users ADD COLUMN full_name TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN bio      TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN location TEXT NOT NULL DEFAULT '';
ALTER TABLE users ADD COLUMN website  TEXT NOT NULL DEFAULT '';
