-- Disposable research fixture, not an application migration.
CREATE TABLE memory_store (
  bot_id text PRIMARY KEY,
  deleted_at timestamptz,
  next_seq bigint NOT NULL DEFAULT 0 CHECK (next_seq >= 0),
  generation integer NOT NULL DEFAULT 1,
  published_seq bigint NOT NULL DEFAULT -1,
  view_revision bigint NOT NULL DEFAULT 0,
  view_nodes jsonb NOT NULL DEFAULT '[]',
  compacting boolean NOT NULL DEFAULT false
);

CREATE TABLE memory_entry (
  bot_id text NOT NULL REFERENCES memory_store ON DELETE CASCADE,
  seq bigint NOT NULL CHECK (seq >= 0),
  request_key text NOT NULL,
  kind text NOT NULL CHECK (kind IN ('user','assistant','tool','echo','work','note')),
  authority text NOT NULL CHECK (authority IN ('owner','external','derived')),
  source_ref text NOT NULL,
  body text NOT NULL,
  body_hash bytea NOT NULL CHECK (body_hash = sha256(convert_to(body, 'UTF8'))),
  search tsvector GENERATED ALWAYS AS (to_tsvector('simple'::regconfig, body)) STORED,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  supersedes_seq bigint,
  PRIMARY KEY (bot_id, seq),
  UNIQUE (bot_id, request_key),
  FOREIGN KEY (bot_id, supersedes_seq) REFERENCES memory_entry(bot_id, seq),
  CHECK (supersedes_seq IS NULL OR supersedes_seq < seq),
  CHECK (supersedes_seq IS NULL OR authority = 'owner')
);
CREATE INDEX memory_entry_search ON memory_entry USING gin(search);

CREATE TABLE memory_node (
  bot_id text NOT NULL REFERENCES memory_store ON DELETE CASCADE,
  generation integer NOT NULL,
  start_seq bigint NOT NULL CHECK (start_seq >= 0),
  span bigint NOT NULL CHECK (span > 0 AND span <= (1::bigint << 60)),
  body text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 512),
  method text NOT NULL CHECK (method IN ('verbatim','join','model','synthetic')),
  input_hash bytea NOT NULL,
  audit jsonb NOT NULL DEFAULT '{}',
  PRIMARY KEY (bot_id, generation, start_seq, span),
  CHECK ((span & (span - 1)) = 0 AND start_seq % span = 0)
);

CREATE TABLE memory_job (
  bot_id text NOT NULL REFERENCES memory_store ON DELETE CASCADE,
  generation integer NOT NULL,
  start_seq bigint NOT NULL,
  span bigint NOT NULL CHECK (span > 0 AND (span & (span - 1)) = 0),
  available_at timestamptz NOT NULL DEFAULT now(),
  lease_token text,
  lease_until timestamptz,
  attempts integer NOT NULL DEFAULT 0,
  PRIMARY KEY (bot_id, generation, start_seq, span),
  CHECK (start_seq >= 0 AND start_seq % span = 0),
  CHECK ((lease_token IS NULL) = (lease_until IS NULL))
);
CREATE INDEX memory_job_ready ON memory_job(available_at, bot_id, start_seq, span);

CREATE FUNCTION immutable_memory() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'memory entries and built nodes are immutable';
END $$;
CREATE TRIGGER memory_entry_immutable BEFORE UPDATE ON memory_entry
  FOR EACH ROW EXECUTE FUNCTION immutable_memory();
CREATE TRIGGER memory_node_immutable BEFORE UPDATE ON memory_node
  FOR EACH ROW EXECUTE FUNCTION immutable_memory();

-- Fixture helper. The Rust API would own this short transaction and validate
-- source identity before it reaches SQL. Never call a model while holding it.
CREATE FUNCTION fixture_append(b text, k text, content text,
  auth text DEFAULT 'owner', superseded bigint DEFAULT NULL)
RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE n bigint; old memory_entry%ROWTYPE;
BEGIN
  SELECT next_seq INTO n FROM memory_store WHERE bot_id=b AND deleted_at IS NULL FOR NO KEY UPDATE;
  IF NOT FOUND THEN RAISE EXCEPTION 'unknown bot'; END IF;
  SELECT * INTO old FROM memory_entry WHERE bot_id=b AND request_key=k;
  IF FOUND THEN
    IF old.body IS DISTINCT FROM content OR old.authority IS DISTINCT FROM auth
      OR old.supersedes_seq IS DISTINCT FROM superseded THEN
      RAISE EXCEPTION 'idempotency key reused for different input';
    END IF;
    RETURN old.seq;
  END IF;
  INSERT INTO memory_entry(bot_id,seq,request_key,kind,authority,source_ref,body,body_hash,supersedes_seq)
    VALUES (b,n,k,'user',auth,'synthetic:'||k,content,sha256(convert_to(content,'UTF8')),superseded);
  UPDATE memory_store SET next_seq=n+1 WHERE bot_id=b;
  INSERT INTO memory_job(bot_id,generation,start_seq,span)
    SELECT b,generation,n,1 FROM memory_store WHERE bot_id=b;
  RETURN n;
END $$;
