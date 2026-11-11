-- Access tokens add and remove their owner's SSH and GPG keys (through the
-- API) only when created for it. A key is lasting access, and over SSH all
-- of an administrator's, which an everyday token mustn't be turned into.
ALTER TABLE access_tokens ADD COLUMN manage_keys INTEGER NOT NULL DEFAULT 0;
