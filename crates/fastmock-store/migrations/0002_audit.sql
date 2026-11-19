create table fm_audit (
    id bigint generated always as identity primary key,
    at timestamptz not null,
    principal text not null,
    role text not null,
    action text not null,
    detail jsonb not null
);

create index fm_audit_at on fm_audit (at);
