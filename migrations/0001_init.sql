-- Initial schema. Amounts are integers in base units (lamports / raw token
-- units). u64 fit in BIGINT only up to 2^63-1, so u64/u128 values that can
-- exceed that use NUMERIC(39,0). Every trade row carries its network.

CREATE TABLE users (
    id            UUID PRIMARY KEY,
    telegram_id   BIGINT NOT NULL UNIQUE,
    username      TEXT,
    beta_allowed  BOOLEAN NOT NULL DEFAULT FALSE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Mini App sessions (only the SHA-256 of the bearer token is stored).
CREATE TABLE telegram_sessions (
    token_hash  TEXT PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at  TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX telegram_sessions_expiry ON telegram_sessions (expires_at);

-- initData replay mitigation: a signed payload mints at most one session.
CREATE TABLE telegram_init_data_seen (
    hash        TEXT PRIMARY KEY,
    expires_at  TIMESTAMPTZ NOT NULL
);

-- Webhook dedupe by update_id.
CREATE TABLE telegram_updates (
    update_id    BIGINT PRIMARY KEY,
    received_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Server-side inline button tokens (user + chat bound, expiring, single-use for spends).
CREATE TABLE callback_tokens (
    token        TEXT PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    telegram_id  BIGINT NOT NULL,
    chat_id      BIGINT NOT NULL,
    action       JSONB NOT NULL,
    single_use   BOOLEAN NOT NULL,
    expires_at   TIMESTAMPTZ NOT NULL,
    used_at      TIMESTAMPTZ
);

CREATE TABLE wallets (
    id           UUID PRIMARY KEY,
    user_id      UUID NOT NULL REFERENCES users(id),
    network      TEXT NOT NULL CHECK (network IN ('mock','localnet','devnet','testnet','mainnet')),
    pubkey       TEXT NOT NULL,
    signer_kind  TEXT NOT NULL CHECK (signer_kind IN ('mock','managed_local','kms','external')),
    label        TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (network, pubkey)
);
CREATE INDEX wallets_user ON wallets (user_id);

-- Envelope-encrypted key material (never plaintext). AAD binds to wallet id.
CREATE TABLE wallet_key_metadata (
    wallet_id    UUID PRIMARY KEY REFERENCES wallets(id),
    version      SMALLINT NOT NULL,
    kek_id       TEXT NOT NULL,
    wrapped_dek  BYTEA NOT NULL,
    sealed_seed  BYTEA NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-wallet hard limits (WalletPolicy JSON).
CREATE TABLE risk_limits (
    wallet_id   UUID PRIMARY KEY REFERENCES wallets(id),
    policy      JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE user_settings (
    user_id     UUID PRIMARY KEY REFERENCES users(id),
    settings    JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Durable intent; (user_id, idempotency_key) is the business-level dedupe.
CREATE TABLE trade_intents (
    id               UUID PRIMARY KEY,
    user_id          UUID NOT NULL REFERENCES users(id),
    wallet_id        UUID NOT NULL REFERENCES wallets(id),
    network          TEXT NOT NULL,
    idempotency_key  TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 1 AND 160),
    intent           JSONB NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (user_id, idempotency_key)
);

CREATE TABLE orders (
    id             UUID PRIMARY KEY REFERENCES trade_intents(id),
    user_id        UUID NOT NULL REFERENCES users(id),
    wallet_id      UUID NOT NULL REFERENCES wallets(id),
    wallet_pubkey  TEXT NOT NULL,
    network        TEXT NOT NULL,
    side           TEXT NOT NULL CHECK (side IN ('buy','sell')),
    mint           TEXT NOT NULL,
    buy_lamports   BIGINT NOT NULL DEFAULT 0 CHECK (buy_lamports >= 0),
    state          TEXT NOT NULL CHECK (state IN ('CREATED','QUOTED','AWAITING_AUTH','SIGNING','SIGNED','SUBMITTED','OBSERVED','CONFIRMED','FINALIZED','FAILED_ONCHAIN','EXPIRED','REJECTED','UNKNOWN_RECONCILE')),
    quote          JSONB,
    reject_reason  TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX orders_user_created ON orders (user_id, created_at DESC);
CREATE INDEX orders_wallet_created ON orders (wallet_id, created_at);
CREATE INDEX orders_unresolved ON orders (state) WHERE state NOT IN ('FINALIZED','FAILED_ONCHAIN','EXPIRED','REJECTED');

-- Immutable state-transition journal.
CREATE TABLE order_events (
    id          BIGSERIAL PRIMARY KEY,
    order_id    UUID NOT NULL REFERENCES orders(id),
    from_state  TEXT,
    to_state    TEXT NOT NULL,
    detail      JSONB NOT NULL DEFAULT '{}'::jsonb,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX order_events_order ON order_events (order_id, id);

-- One signed payload per order; signature globally unique.
CREATE TABLE transactions (
    signature                TEXT PRIMARY KEY,
    order_id                 UUID NOT NULL UNIQUE REFERENCES orders(id),
    network                  TEXT NOT NULL,
    signed                   JSONB NOT NULL,
    last_valid_block_height  BIGINT NOT NULL CHECK (last_valid_block_height >= 0),
    tip_lamports             BIGINT NOT NULL DEFAULT 0 CHECK (tip_lamports >= 0),
    fill                     JSONB,
    landed_slot              BIGINT,
    fee_lamports             BIGINT CHECK (fee_lamports IS NULL OR fee_lamports >= 0),
    signed_at                TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE sender_attempts (
    id           BIGSERIAL PRIMARY KEY,
    order_id     UUID NOT NULL REFERENCES orders(id),
    signature    TEXT NOT NULL,
    sender       TEXT NOT NULL,
    endpoint     TEXT NOT NULL,
    outcome      TEXT NOT NULL,
    detail       JSONB NOT NULL,
    latency_us   BIGINT NOT NULL CHECK (latency_us >= 0),
    rebroadcast  BOOLEAN NOT NULL DEFAULT FALSE,
    wall_start   TIMESTAMPTZ NOT NULL
);
CREATE INDEX sender_attempts_order ON sender_attempts (order_id);

-- Balance snapshots (P1 cache; not authoritative — chain is).
CREATE TABLE balances (
    wallet_id   UUID NOT NULL REFERENCES wallets(id),
    mint        TEXT NOT NULL,
    raw         NUMERIC(39,0) NOT NULL CHECK (raw >= 0),
    slot        BIGINT NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (wallet_id, mint)
);

-- Weighted-average positions.
CREATE TABLE positions (
    wallet_id      UUID NOT NULL REFERENCES wallets(id),
    mint           TEXT NOT NULL,
    user_id        UUID NOT NULL REFERENCES users(id),
    qty_raw        NUMERIC(39,0) NOT NULL CHECK (qty_raw >= 0),
    cost_basis     NUMERIC(39,0) NOT NULL CHECK (cost_basis >= 0),
    realized_pnl   NUMERIC(40,0) NOT NULL,
    fees_paid      NUMERIC(39,0) NOT NULL CHECK (fees_paid >= 0),
    rent_locked    NUMERIC(39,0) NOT NULL CHECK (rent_locked >= 0),
    gross_bought   NUMERIC(39,0) NOT NULL CHECK (gross_bought >= 0),
    gross_sold     NUMERIC(39,0) NOT NULL CHECK (gross_sold >= 0),
    buys           INTEGER NOT NULL CHECK (buys >= 0),
    sells          INTEGER NOT NULL CHECK (sells >= 0),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (wallet_id, mint)
);

-- Immutable fill ledger; UNIQUE(order_id) makes position updates idempotent.
CREATE TABLE position_lots (
    id         BIGSERIAL PRIMARY KEY,
    order_id   UUID NOT NULL UNIQUE REFERENCES orders(id),
    wallet_id  UUID NOT NULL REFERENCES wallets(id),
    mint       TEXT NOT NULL,
    side       TEXT NOT NULL CHECK (side IN ('buy','sell')),
    qty_raw    NUMERIC(39,0) NOT NULL CHECK (qty_raw > 0),
    lamports   NUMERIC(39,0) NOT NULL CHECK (lamports >= 0),
    fees       NUMERIC(39,0) NOT NULL CHECK (fees >= 0),
    rent       NUMERIC(39,0) NOT NULL CHECK (rent >= 0),
    at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tracked_wallets (
    user_id     UUID NOT NULL REFERENCES users(id),
    address     TEXT NOT NULL,
    label       TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, address)
);

-- P1 (not used by this build): copy trading. Disabled by default.
CREATE TABLE copy_rules (
    id              UUID PRIMARY KEY,
    user_id         UUID NOT NULL REFERENCES users(id),
    source_address  TEXT NOT NULL,
    config          JSONB NOT NULL,
    enabled         BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Source-event uniqueness: one follower action per (rule, source signature, action).
CREATE TABLE copy_executions (
    rule_id           UUID NOT NULL REFERENCES copy_rules(id),
    source_signature  TEXT NOT NULL,
    action            TEXT NOT NULL,
    order_id          UUID REFERENCES orders(id),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (rule_id, source_signature, action)
);

-- P1 (not used by this build): off-chain TP/SL watcher.
CREATE TABLE conditional_orders (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id),
    wallet_id   UUID NOT NULL REFERENCES wallets(id),
    mint        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('take_profit','stop_loss','limit','trailing')),
    params      JSONB NOT NULL,
    state       TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE fee_records (
    id        BIGSERIAL PRIMARY KEY,
    order_id  UUID NOT NULL REFERENCES orders(id),
    kind      TEXT NOT NULL CHECK (kind IN ('network','priority','tip','protocol','platform','rent')),
    lamports  BIGINT NOT NULL CHECK (lamports >= 0),
    at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- P2 (not used): referrals.
CREATE TABLE referral_codes (
    code        TEXT PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE referral_earnings (
    id        BIGSERIAL PRIMARY KEY,
    code      TEXT NOT NULL REFERENCES referral_codes(code),
    order_id  UUID NOT NULL REFERENCES orders(id),
    lamports  BIGINT NOT NULL CHECK (lamports >= 0),
    at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (code, order_id)
);

CREATE TABLE audit_events (
    id       BIGSERIAL PRIMARY KEY,
    user_id  UUID,
    kind     TEXT NOT NULL,
    detail   JSONB NOT NULL,
    at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE outbox_notifications (
    id                   UUID PRIMARY KEY,
    chat_id              BIGINT NOT NULL,
    text                 TEXT NOT NULL,
    buttons              JSONB,
    dedupe_key           TEXT NOT NULL UNIQUE,
    attempts             INTEGER NOT NULL DEFAULT 0,
    next_attempt_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    edit_message_id      BIGINT,
    sent_at              TIMESTAMPTZ,
    telegram_message_id  BIGINT,
    last_error           TEXT
);
CREATE INDEX outbox_due ON outbox_notifications (next_attempt_at) WHERE sent_at IS NULL;

CREATE TABLE order_messages (
    order_id    UUID PRIMARY KEY REFERENCES orders(id),
    chat_id     BIGINT NOT NULL,
    message_id  BIGINT NOT NULL
);

-- Append-only enforcement for journals.
CREATE FUNCTION forbid_mutation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION '% is append-only', TG_TABLE_NAME;
END $$;
CREATE TRIGGER order_events_append_only BEFORE UPDATE OR DELETE ON order_events FOR EACH ROW EXECUTE FUNCTION forbid_mutation();
CREATE TRIGGER audit_events_append_only BEFORE UPDATE OR DELETE ON audit_events FOR EACH ROW EXECUTE FUNCTION forbid_mutation();
CREATE TRIGGER position_lots_append_only BEFORE UPDATE OR DELETE ON position_lots FOR EACH ROW EXECUTE FUNCTION forbid_mutation();

-- Legal state transitions are also enforced in the database (mirror of
-- trader_domain::OrderState::can_transition_to; tested by crates/api/tests/pg.rs).
CREATE FUNCTION check_order_transition() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE ok BOOLEAN;
BEGIN
    IF NEW.state = OLD.state THEN RETURN NEW; END IF;
    ok := CASE OLD.state
        WHEN 'CREATED' THEN NEW.state IN ('QUOTED','REJECTED')
        WHEN 'QUOTED' THEN NEW.state IN ('AWAITING_AUTH','SIGNING','REJECTED')
        WHEN 'AWAITING_AUTH' THEN NEW.state IN ('QUOTED','SIGNING','REJECTED')
        WHEN 'SIGNING' THEN NEW.state IN ('SIGNED','REJECTED')
        WHEN 'SIGNED' THEN NEW.state IN ('SUBMITTED','REJECTED','EXPIRED','UNKNOWN_RECONCILE','OBSERVED','CONFIRMED','FINALIZED','FAILED_ONCHAIN')
        WHEN 'SUBMITTED' THEN NEW.state IN ('OBSERVED','CONFIRMED','FINALIZED','FAILED_ONCHAIN','EXPIRED','UNKNOWN_RECONCILE')
        WHEN 'OBSERVED' THEN NEW.state IN ('CONFIRMED','FINALIZED','FAILED_ONCHAIN','SUBMITTED','EXPIRED','UNKNOWN_RECONCILE')
        WHEN 'CONFIRMED' THEN NEW.state IN ('FINALIZED','UNKNOWN_RECONCILE')
        WHEN 'UNKNOWN_RECONCILE' THEN NEW.state IN ('SUBMITTED','OBSERVED','CONFIRMED','FINALIZED','FAILED_ONCHAIN','EXPIRED')
        ELSE FALSE
    END;
    IF NOT ok THEN
        RAISE EXCEPTION 'illegal order transition % -> %', OLD.state, NEW.state USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER orders_transition BEFORE UPDATE OF state ON orders FOR EACH ROW EXECUTE FUNCTION check_order_transition();
