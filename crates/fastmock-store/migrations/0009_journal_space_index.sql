-- no-transaction
create index concurrently if not exists fm_journal_space_id on fm_journal (space, id desc) where space is not null;
