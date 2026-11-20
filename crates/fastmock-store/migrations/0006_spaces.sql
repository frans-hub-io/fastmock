alter table fm_stubs add column if not exists space text;
alter table fm_journal add column if not exists space text;
alter table fm_outbox add column if not exists space text;

create index if not exists fm_stubs_space on fm_stubs (space) where space is not null;
create index if not exists fm_journal_space_id on fm_journal (space, id desc) where space is not null;
create index if not exists fm_outbox_space_created on fm_outbox (space, created_at desc) where space is not null;
