-- Add migration script here
create table records (
	id uuid primary key,            -- remember to use uuidv7 for happy indices <3
	host uuid not null,             -- a unique identifier for the host
	parent uuid not null,           -- the ID of the parent record, bearing in mind this is a linked list
	timestamp bigint not null,      -- not a timestamp type, as those do not have nanosecond precision
	version text not null,
	tag text not null,              -- what is this? history, kv, whatever. Remember clients get a log per tag per host
	data bytea not null,            -- store the actual history data, encrypted. I don't wanna know!

	user_id bigint not null,        -- allow multiple users
	created_at timestamp not null default current_timestamp
);
