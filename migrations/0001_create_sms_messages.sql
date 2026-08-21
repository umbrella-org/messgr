CREATE TABLE IF NOT EXISTS sms_messages (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    sender VARCHAR(32) NOT NULL,
    recipient VARCHAR(32) NOT NULL,
    body TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
