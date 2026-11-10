-- Whether a scan of the source also imports the forks it finds.
ALTER TABLE sources ADD COLUMN include_forks INTEGER NOT NULL DEFAULT 0;
