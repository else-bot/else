-- A background job that likely ran out of its computer's memory: the guest
-- kernel's OOM killer ran while it failed, or its output reported a failed
-- allocation.
ALTER TABLE guest_jobs ADD COLUMN out_of_memory INTEGER NOT NULL DEFAULT 0 CHECK (out_of_memory IN (0, 1));
-- The boot memory the owner approved for this computer; NULL boots it at the
-- configured size.
ALTER TABLE bot_computers ADD COLUMN chosen_memory_mib INTEGER CHECK (chosen_memory_mib BETWEEN 256 AND 262144);
