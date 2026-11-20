-- no-transaction
create index concurrently if not exists fm_stubs_space on fm_stubs (space) where space is not null;
