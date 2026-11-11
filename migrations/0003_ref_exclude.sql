-- Ref patterns that a fetch leaves out, one per line. Refs already in the
-- archive stay; a pattern only stops new fetches of matching refs.
ALTER TABLE hosts ADD COLUMN ref_exclude TEXT;
ALTER TABLE repos ADD COLUMN ref_exclude TEXT;
