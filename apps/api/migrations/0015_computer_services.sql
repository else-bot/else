-- POC-018 review: the generation whose boot started the desktop services.
-- Until it equals the computer's generation the services are retried, and
-- the owner's desktop is not offered.
ALTER TABLE bot_computers ADD COLUMN services_generation bigint;
