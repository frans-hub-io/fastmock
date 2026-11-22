-- no-transaction
create index concurrently if not exists fm_captured_messages_space_idx on fm_captured_messages (space, id);
