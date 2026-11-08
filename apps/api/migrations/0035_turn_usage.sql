-- Tokens a turn's provider reported, summed over its attempts. Input counts
-- every prompt token, cached ones included; NULL when nothing was reported.
ALTER TABLE turns ADD COLUMN input_tokens INTEGER CHECK (input_tokens >= 0);
ALTER TABLE turns ADD COLUMN cached_input_tokens INTEGER CHECK (cached_input_tokens >= 0);
ALTER TABLE turns ADD COLUMN cache_write_tokens INTEGER CHECK (cache_write_tokens >= 0);
ALTER TABLE turns ADD COLUMN output_tokens INTEGER CHECK (output_tokens >= 0);
