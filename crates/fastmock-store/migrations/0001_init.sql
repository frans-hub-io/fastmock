create table fm_stubs (
    id uuid primary key,
    definition jsonb not null,
    priority integer not null,
    tags text[] not null default '{}',
    updated_at timestamptz not null default now()
);

create index fm_stubs_tags on fm_stubs using gin (tags);

create table fm_revision (
    id smallint primary key check (id = 1),
    revision bigint not null
);

insert into fm_revision (id, revision) values (1, 0);

create table fm_journal (
    id bigint generated always as identity primary key,
    at timestamptz not null,
    source text not null,
    method text not null,
    path text not null,
    query text not null,
    headers jsonb not null,
    body text not null,
    status integer not null,
    response_body text not null,
    stub_id uuid,
    correlation text,
    matched boolean not null,
    duration_ms bigint not null,
    trace_id text
);

create index fm_journal_at on fm_journal (at);
create index fm_journal_source_at on fm_journal (source, at desc);
create index fm_journal_correlation on fm_journal (correlation) where correlation is not null;

create table fm_outbox (
    id uuid primary key,
    source text not null,
    correlation text,
    url text not null,
    method text not null,
    headers jsonb not null,
    body text not null,
    due_at timestamptz not null,
    attempts integer not null default 0,
    max_attempts integer not null,
    status text not null check (status in ('pending', 'sent', 'failed')),
    last_error text,
    last_status integer,
    created_at timestamptz not null,
    sent_at timestamptz,
    lease_until timestamptz,
    traceparent text
);

create index fm_outbox_due on fm_outbox (due_at) where status = 'pending';
create index fm_outbox_correlation on fm_outbox (correlation) where correlation is not null;
create index fm_outbox_created on fm_outbox (created_at);

create table fm_data (
    namespace text not null,
    key text not null,
    value jsonb not null,
    updated_at timestamptz not null default now(),
    primary key (namespace, key)
);
