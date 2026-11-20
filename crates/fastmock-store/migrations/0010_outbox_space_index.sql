-- no-transaction
create index concurrently if not exists fm_outbox_space_created on fm_outbox (space, created_at desc) where space is not null;
