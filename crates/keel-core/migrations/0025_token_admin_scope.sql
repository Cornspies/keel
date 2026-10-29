-- Access tokens of site administrators carry administration (every
-- repository, the admin API) only when created for it, so that an
-- everyday CI token isn't a key to everything.
ALTER TABLE access_tokens ADD COLUMN site_admin INTEGER NOT NULL DEFAULT 0;
