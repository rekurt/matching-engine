CREATE TABLE assets (
    code TEXT PRIMARY KEY CHECK (code ~ '^[A-Z0-9]{1,16}$'),
    funding_precision SMALLINT NOT NULL DEFAULT 8 CHECK (funding_precision BETWEEN 0 AND 20)
);
CREATE TABLE accounts (
    id UUID PRIMARY KEY,
    active BOOLEAN NOT NULL DEFAULT TRUE
);
CREATE TABLE wallets (
    id TEXT PRIMARY KEY,
    account_id UUID REFERENCES accounts(id),
    asset TEXT NOT NULL REFERENCES assets(code),
    bucket TEXT NOT NULL CHECK (bucket IN ('available','reserved','external','fees')),
    balance NUMERIC(78,0) NOT NULL DEFAULT 0,
    CHECK ((account_id IS NULL) = (bucket IN ('external','fees'))),
    CHECK (bucket = 'external' OR balance >= 0),
    UNIQUE NULLS NOT DISTINCT (account_id, asset, bucket)
);
CREATE TABLE journals (
    id UUID PRIMARY KEY,
    dedup_key TEXT NOT NULL UNIQUE,
    payload_hash TEXT NOT NULL CHECK (length(payload_hash) = 64),
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_txid BIGINT NOT NULL DEFAULT txid_current(),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE postings (
    journal_id UUID NOT NULL REFERENCES journals(id),
    ordinal INTEGER NOT NULL,
    wallet_id TEXT NOT NULL REFERENCES wallets(id),
    amount NUMERIC(78,0) NOT NULL CHECK (amount <> 0),
    PRIMARY KEY (journal_id, ordinal)
);
CREATE FUNCTION reject_journal_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'financial history is immutable' USING ERRCODE = '23514';
END $$;
CREATE TRIGGER journals_immutable BEFORE UPDATE OR DELETE ON journals
FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER postings_immutable BEFORE UPDATE OR DELETE ON postings
FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE FUNCTION guard_posting_insert() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT created_txid FROM journals WHERE id = NEW.journal_id) IS DISTINCT FROM txid_current() THEN
        RAISE EXCEPTION 'cannot append to a committed journal' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER posting_same_transaction BEFORE INSERT ON postings
FOR EACH ROW EXECUTE FUNCTION guard_posting_insert();
CREATE FUNCTION check_journal_balance() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (SELECT count(*) FROM postings WHERE journal_id = NEW.id) < 2
       OR EXISTS (
         SELECT w.asset FROM postings p JOIN wallets w ON w.id = p.wallet_id
         WHERE p.journal_id = NEW.id GROUP BY w.asset HAVING sum(p.amount) <> 0
       ) THEN
        RAISE EXCEPTION 'unbalanced financial journal' USING ERRCODE = '23514';
    END IF;
    RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER balanced_journal AFTER INSERT ON journals
DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION check_journal_balance();
CREATE TABLE fee_schedules (
    id UUID PRIMARY KEY,
    maker_bps INTEGER NOT NULL CHECK (maker_bps BETWEEN 0 AND 10000),
    taker_bps INTEGER NOT NULL CHECK (taker_bps BETWEEN 0 AND 10000)
);
CREATE TRIGGER fee_schedule_immutable BEFORE UPDATE OR DELETE ON fee_schedules
FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE TABLE markets (
    symbol INTEGER PRIMARY KEY CHECK (symbol BETWEEN 0 AND 65535),
    base TEXT NOT NULL REFERENCES assets(code),
    quote TEXT NOT NULL REFERENCES assets(code),
    fee_schedule UUID NOT NULL REFERENCES fee_schedules(id),
    definition_hash TEXT NOT NULL,
    CHECK (base <> quote)
);
CREATE TABLE requests (
    id UUID PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    account_id UUID NOT NULL REFERENCES accounts(id),
    idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 128),
    payload_hash TEXT NOT NULL,
    operation TEXT NOT NULL,
    payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','accepted','rejected','reconciliation')),
    initial_response JSONB,
    UNIQUE (account_id, idempotency_key)
);
CREATE TABLE orders (
    id BIGSERIAL PRIMARY KEY,
    account_id UUID NOT NULL REFERENCES accounts(id),
    symbol INTEGER NOT NULL REFERENCES markets(symbol),
    fee_schedule UUID NOT NULL REFERENCES fee_schedules(id),
    version BIGINT NOT NULL DEFAULT 1 CHECK (version > 0),
    spec JSONB NOT NULL,
    remaining NUMERIC(20,0) NOT NULL CHECK (remaining >= 0),
    held NUMERIC(78,0) NOT NULL CHECK (held >= 0),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','open','filled','cancelled','rejected')),
    pending_request UUID REFERENCES requests(id)
);
CREATE TABLE order_versions (
    order_id BIGINT NOT NULL REFERENCES orders(id),
    version BIGINT NOT NULL CHECK (version > 0),
    spec JSONB NOT NULL,
    request_id UUID NOT NULL UNIQUE REFERENCES requests(id),
    PRIMARY KEY (order_id,version)
);
CREATE TRIGGER order_versions_immutable BEFORE UPDATE OR DELETE ON order_versions
FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE TABLE outbox (
    sequence BIGSERIAL PRIMARY KEY,
    request_id UUID NOT NULL UNIQUE REFERENCES requests(id),
    engine_request_id BIGSERIAL NOT NULL UNIQUE,
    payload JSONB NOT NULL,
    encoded_frame BYTEA,
    completed BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE TABLE engine_binding (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    engine_id UUID NOT NULL,
    definition_hash TEXT NOT NULL,
    maintenance BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE TABLE cursors (
    engine_id UUID NOT NULL,
    shard INTEGER NOT NULL CHECK (shard >= 0),
    sequence NUMERIC(20,0) NOT NULL CHECK (sequence >= 0),
    digest TEXT NOT NULL CHECK (length(digest)=64),
    PRIMARY KEY(engine_id,shard)
);
CREATE TABLE decisions (
    engine_id UUID NOT NULL,
    shard INTEGER NOT NULL,
    sequence NUMERIC(20,0) NOT NULL,
    digest TEXT NOT NULL,
    payload JSONB NOT NULL,
    PRIMARY KEY(engine_id,shard,sequence)
);
CREATE TRIGGER decisions_immutable BEFORE UPDATE OR DELETE ON decisions
FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE TABLE financial_schema (
    singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    version INTEGER NOT NULL CHECK (version > 0)
);
INSERT INTO financial_schema(singleton,version) VALUES(TRUE,1);
ALTER TABLE engine_binding ADD COLUMN reconciliation_reason TEXT;
CREATE TRIGGER journals_no_truncate BEFORE TRUNCATE ON journals
FOR EACH STATEMENT EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER postings_no_truncate BEFORE TRUNCATE ON postings
FOR EACH STATEMENT EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER versions_no_truncate BEFORE TRUNCATE ON order_versions
FOR EACH STATEMENT EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER fees_no_truncate BEFORE TRUNCATE ON fee_schedules
FOR EACH STATEMENT EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER decisions_no_truncate BEFORE TRUNCATE ON decisions
FOR EACH STATEMENT EXECUTE FUNCTION reject_journal_mutation();

CREATE FUNCTION guard_request_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.id,NEW.account_id,NEW.idempotency_key,NEW.payload_hash,NEW.operation,NEW.payload,NEW.created_at)
       IS DISTINCT FROM (OLD.id,OLD.account_id,OLD.idempotency_key,OLD.payload_hash,OLD.operation,OLD.payload,OLD.created_at)
       OR (OLD.initial_response IS NOT NULL AND NEW.initial_response IS DISTINCT FROM OLD.initial_response)
       OR (OLD.status IN ('accepted','rejected') AND NEW.status IS DISTINCT FROM OLD.status) THEN
        RAISE EXCEPTION 'request identity/response is immutable' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER requests_identity BEFORE UPDATE ON requests FOR EACH ROW EXECUTE FUNCTION guard_request_update();
CREATE FUNCTION guard_outbox_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.sequence,NEW.request_id,NEW.engine_request_id,NEW.payload) IS DISTINCT FROM (OLD.sequence,OLD.request_id,OLD.engine_request_id,OLD.payload)
       OR (OLD.encoded_frame IS NOT NULL AND NEW.encoded_frame IS DISTINCT FROM OLD.encoded_frame)
       OR (OLD.completed AND NOT NEW.completed) THEN
        RAISE EXCEPTION 'outbox identity/bytes are immutable' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER outbox_identity BEFORE UPDATE ON outbox FOR EACH ROW EXECUTE FUNCTION guard_outbox_update();
CREATE INDEX orders_account ON orders(account_id,id);
CREATE TABLE account_keys (
    key TEXT PRIMARY KEY,
    payload_hash TEXT NOT NULL,
    account_id UUID NOT NULL UNIQUE REFERENCES accounts(id)
);
CREATE TRIGGER account_keys_immutable BEFORE UPDATE OR DELETE ON account_keys FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER requests_no_delete BEFORE DELETE ON requests FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE TRIGGER outbox_no_delete BEFORE DELETE ON outbox FOR EACH ROW EXECUTE FUNCTION reject_journal_mutation();
CREATE INDEX outbox_pending ON outbox(sequence) WHERE NOT completed;
CREATE INDEX requests_pending ON requests(id) WHERE status='pending';
