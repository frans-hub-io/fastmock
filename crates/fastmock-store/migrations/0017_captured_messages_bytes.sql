alter table fm_captured_messages add column if not exists bytes bigint not null default 0;

update fm_captured_messages set bytes = octet_length(body) + octet_length(entry::text) where bytes = 0;
