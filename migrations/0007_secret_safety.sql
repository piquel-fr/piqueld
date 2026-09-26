-- Cleanup reservations survive cancellation, partial Docker failures and restarts.
ALTER TABLE application_secrets ADD COLUMN deletion_id TEXT;

-- Keep the key binding even after the final secret is deleted.
CREATE TABLE secret_key_verification (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL
);
