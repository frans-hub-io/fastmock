alter table fm_stubs add column if not exists space text;
alter table fm_journal add column if not exists space text;
alter table fm_outbox add column if not exists space text;
