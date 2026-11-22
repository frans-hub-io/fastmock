create table if not exists fm_captured_messages (
    id uuid primary key,
    space text,
    source_id uuid not null,
    protocol text not null,
    topic text not null,
    received_at timestamptz not null,
    entry jsonb not null,
    body bytea not null
);

create index if not exists fm_captured_messages_source_idx on fm_captured_messages (source_id, id);
create index if not exists fm_captured_messages_received_idx on fm_captured_messages (received_at);
