ALTER TABLE accounts
    ADD COLUMN account_state_version INTEGER NOT NULL DEFAULT 0;
