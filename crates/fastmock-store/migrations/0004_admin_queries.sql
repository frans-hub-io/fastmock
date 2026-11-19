create index if not exists fm_journal_stub_id on fm_journal (stub_id, at) where stub_id is not null;
create index if not exists fm_audit_action on fm_audit (action, id desc);
