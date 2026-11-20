create sequence if not exists fm_mail_uid_validity;
select setval('fm_mail_uid_validity', greatest((select last_value from fm_mail_uid_validity), extract(epoch from now())::bigint));

create table if not exists fm_mail_messages (
    id uuid primary key,
    space text,
    received_at timestamptz not null,
    source text not null,
    provider text,
    envelope jsonb not null,
    summary jsonb not null,
    labels text[] not null default '{}',
    provider_extras jsonb,
    transcript jsonb not null default '[]',
    search_text text not null default '',
    subject_lower text,
    from_terms text[] not null default '{}',
    to_terms text[] not null default '{}',
    cc_terms text[] not null default '{}',
    has_attachments boolean not null default false,
    raw_size bigint not null,
    raw bytea not null
);

create table if not exists fm_mail_mailboxes (
    id bigint generated always as identity primary key,
    area text not null,
    address text not null,
    password_hash text,
    unique (area, address)
);

create table if not exists fm_mail_folders (
    id bigint generated always as identity primary key,
    mailbox_id bigint not null references fm_mail_mailboxes (id) on delete cascade,
    name text not null,
    uid_validity bigint not null,
    uid_next bigint not null default 1,
    unique (mailbox_id, name)
);

create table if not exists fm_mail_entries (
    folder_id bigint not null references fm_mail_folders (id) on delete cascade,
    uid bigint not null,
    message_id uuid not null references fm_mail_messages (id) on delete cascade,
    flags text[] not null default '{}',
    primary key (folder_id, uid)
);
