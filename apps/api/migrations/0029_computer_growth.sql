ALTER TABLE bot_computers ADD COLUMN max_cpus INTEGER CHECK (max_cpus BETWEEN 1 AND 256);
ALTER TABLE bot_computers ADD COLUMN max_memory_mib INTEGER CHECK (max_memory_mib BETWEEN 256 AND 4194304);
ALTER TABLE bot_computers ADD COLUMN grown INTEGER NOT NULL DEFAULT 0 CHECK (grown IN (0, 1));
ALTER TABLE bot_computers ADD COLUMN desktop_generation INTEGER;
