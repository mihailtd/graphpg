-- Forum fixture: users, forums and messages. Adapted from the Graphile Crystal test fixtures (MIT).
CREATE EXTENSION IF NOT EXISTS "uuid-ossp";
CREATE EXTENSION IF NOT EXISTS citext;
SET check_function_bodies = false;

CREATE SCHEMA app_public;
CREATE TABLE app_public.messages (
    id uuid DEFAULT public.uuid_generate_v1mc() NOT NULL,
    forum_id uuid NOT NULL,
    author_id uuid NOT NULL,
    body text NOT NULL,
    featured boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    archived_at timestamp with time zone
);
CREATE FUNCTION app_public.featured_messages() RETURNS SETOF app_public.messages
    LANGUAGE sql STABLE
    AS $$
  select messages.*
  from app_public.messages
  where featured is true
$$;
CREATE FUNCTION app_public.forum_names() RETURNS SETOF text
    LANGUAGE sql STABLE
    AS $$
  select name from app_public.forums order by name asc;
$$;
CREATE FUNCTION app_public.forum_names_array() RETURNS text[]
    LANGUAGE sql STABLE
    AS $$
  select array_agg(name order by name asc) from app_public.forums;
$$;
CREATE FUNCTION app_public.forum_names_cases() RETURNS SETOF text[]
    LANGUAGE sql STABLE
    AS $$
  select array[name, lower(name), upper(name)] as cases from app_public.forums order by name asc;
$$;
CREATE TABLE app_public.forums (
    id uuid DEFAULT public.uuid_generate_v1mc() NOT NULL,
    name text NOT NULL,
    archived_at timestamp with time zone
);
CREATE FUNCTION app_public.forums_featured_messages(forum app_public.forums) RETURNS SETOF app_public.messages
    LANGUAGE sql STABLE
    AS $$
  select messages.*
  from app_public.messages
  where featured is true
  and messages.forum_id = forum.id
  order by messages.id
$$;
CREATE FUNCTION app_public.forums_messages_list_set(forum app_public.forums) RETURNS SETOF app_public.messages[]
    LANGUAGE sql STABLE
    AS $$
  select array_agg(messages.* order by messages.id)
  from app_public.messages
  where messages.forum_id = forum.id
  group by featured
  order by featured
$$;
CREATE TABLE app_public.users (
    id uuid DEFAULT public.uuid_generate_v1mc() NOT NULL,
    username public.citext NOT NULL,
    gravatar_url text,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);
CREATE FUNCTION app_public.forums_random_user(forum app_public.forums) RETURNS app_public.users
    LANGUAGE sql STABLE
    AS $$
  select users.*
  from app_public.users
  inner join app_public.messages
  on messages.forum_id = forum.id
  where users.id = 'b0b00000-0000-0000-0000-000000000b0b' /* user chosen by a fair dice role - 1-2 Alice, 3-4 Bob, 5-6 Cecilia */
  limit 1;
$$;
CREATE FUNCTION app_public.forums_unique_author_count(forum app_public.forums, featured boolean DEFAULT NULL::boolean) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
  select count(distinct author_id)::int
  from app_public.messages
  where forum_id = forum.id
  and (
    forums_unique_author_count.featured is null
  or
    (forums_unique_author_count.featured is true and messages.featured is true)
  or
    (forums_unique_author_count.featured is false and not exists (
      select 1
      from app_public.messages m2
      where m2.author_id = messages.author_id
      and forum_id = forum.id
      and m2.featured is true
    ))
  )
$$;
CREATE FUNCTION app_public.random_user() RETURNS app_public.users
    LANGUAGE sql STABLE
    AS $$
  select users.*
  from app_public.users
  where users.id = 'b0b00000-0000-0000-0000-000000000b0b' /* user chosen by a fair dice role - 1-2 Alice, 3-4 Bob, 5-6 Cecilia */
  limit 1
$$;
CREATE FUNCTION app_public.random_user_array() RETURNS app_public.users[]
    LANGUAGE sql STABLE
    AS $$
  select array_agg(users order by users.id)
  from app_public.users
  where users.id = 'b0b00000-0000-0000-0000-000000000b0b' /* user chosen by a fair dice role - 1-2 Alice, 3-4 Bob, 5-6 Cecilia */
$$;
CREATE FUNCTION app_public.random_user_array_set() RETURNS SETOF app_public.users[]
    LANGUAGE sql STABLE
    AS $$
  select array_agg(users order by users.id)
  from generate_series(0, 1) i
  inner join app_public.users
  on (
    case when i = 0 then
      users.id = 'b0b00000-0000-0000-0000-000000000b0b'
    else
      users.id in ('a11ce000-0000-0000-0000-0000000a11ce', 'cec111a0-0000-0000-0000-00000cec111a')
    end
  )
  group by i
$$;
CREATE FUNCTION app_public.tg_messages__announce() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
begin
  perform pg_notify('forum:' || NEW.forum_id::text || ':message', json_build_object('id', NEW.id, 'op', TG_OP)::text);
  return NEW;
end;
$$;
CREATE FUNCTION app_public.unique_author_count(featured boolean DEFAULT NULL::boolean) RETURNS integer
    LANGUAGE sql STABLE
    AS $$
  select count(distinct author_id)::int
  from app_public.messages
  where (
    unique_author_count.featured is null
  or
    (unique_author_count.featured is true and messages.featured is true)
  or
    (unique_author_count.featured is false and not exists (
      select 1
      from app_public.messages m2
      where m2.author_id = messages.author_id
      and m2.featured is true
    ))
  )
$$;
CREATE FUNCTION app_public.users_most_recent_forum(u app_public.users) RETURNS app_public.forums
    LANGUAGE sql STABLE
    AS $$
  select forums.*
  from app_public.forums
  inner join app_public.messages
  on messages.forum_id = forums.id
  where messages.author_id = u.id
  order by messages.created_at desc
  limit 1;
$$;
INSERT INTO app_public.forums VALUES ('ca700000-0000-0000-0000-000000000ca7', 'Cats', NULL);
INSERT INTO app_public.forums VALUES ('d0900000-0000-0000-0000-000000000d09', 'Dogs', '2026-09-25 21:57:52.34598+00');
INSERT INTO app_public.forums VALUES ('f1700000-0000-0000-0000-000000000f17', 'Postgres', NULL);
INSERT INTO app_public.messages VALUES ('ca70ca70-0000-0000-0000-a11ce000ca70', 'ca700000-0000-0000-0000-000000000ca7', 'a11ce000-0000-0000-0000-0000000a11ce', 'Cats = awesome -- Alice', false, '2000-01-01 00:01:00+00', NULL);
INSERT INTO app_public.messages VALUES ('ca70ca70-0000-0000-0000-b0b00000ca70', 'ca700000-0000-0000-0000-000000000ca7', 'b0b00000-0000-0000-0000-000000000b0b', 'Cats = awesome -- Bob', false, '2000-01-01 00:02:00+00', NULL);
INSERT INTO app_public.messages VALUES ('ca70ca70-0000-0000-0000-cec111a0ca70', 'ca700000-0000-0000-0000-000000000ca7', 'cec111a0-0000-0000-0000-00000cec111a', 'Cats = awesome -- Cecilia', false, '2000-01-01 00:03:00+00', NULL);
INSERT INTO app_public.messages VALUES ('d090d090-0000-0000-0000-a11ce000d090', 'd0900000-0000-0000-0000-000000000d09', 'a11ce000-0000-0000-0000-0000000a11ce', 'Dogs = awesome -- Alice', false, '2000-01-01 00:04:00+00', '2026-09-25 21:57:52.347154+00');
INSERT INTO app_public.messages VALUES ('d090d090-0000-0000-0000-b0b00000d090', 'd0900000-0000-0000-0000-000000000d09', 'b0b00000-0000-0000-0000-000000000b0b', 'Dogs = awesome -- Bob', true, '2000-01-01 00:05:00+00', '2026-09-25 21:57:52.347154+00');
INSERT INTO app_public.messages VALUES ('d090d090-0000-0000-0000-cec111a0d090', 'd0900000-0000-0000-0000-000000000d09', 'cec111a0-0000-0000-0000-00000cec111a', 'Dogs = awesome -- Cecilia', true, '2000-01-01 00:06:00+00', '2026-09-25 21:57:52.347154+00');
INSERT INTO app_public.messages VALUES ('f170f170-0000-0000-0000-a11ce000f170', 'f1700000-0000-0000-0000-000000000f17', 'a11ce000-0000-0000-0000-0000000a11ce', 'Postgres = awesome -- Alice', false, '2000-01-01 00:07:00+00', NULL);
INSERT INTO app_public.messages VALUES ('f170f170-0000-0000-0000-b0b00000f170', 'f1700000-0000-0000-0000-000000000f17', 'b0b00000-0000-0000-0000-000000000b0b', 'Postgres = awesome -- Bob', true, '2000-01-01 00:08:00+00', NULL);
INSERT INTO app_public.messages VALUES ('f170f170-0000-0000-0000-cec111a0f170', 'f1700000-0000-0000-0000-000000000f17', 'cec111a0-0000-0000-0000-00000cec111a', 'Postgres = awesome -- Cecilia', false, '2000-01-01 00:09:00+00', NULL);
INSERT INTO app_public.users VALUES ('a11ce000-0000-0000-0000-0000000a11ce', 'Alice', NULL, '2026-09-25 21:57:52.34459+00');
INSERT INTO app_public.users VALUES ('b0b00000-0000-0000-0000-000000000b0b', 'Bob', NULL, '2026-09-25 21:57:52.34459+00');
INSERT INTO app_public.users VALUES ('cec111a0-0000-0000-0000-00000cec111a', 'Cecilia', NULL, '2026-09-25 21:57:52.34459+00');
ALTER TABLE ONLY app_public.forums
    ADD CONSTRAINT forums_pkey PRIMARY KEY (id);
ALTER TABLE ONLY app_public.messages
    ADD CONSTRAINT messages_pkey PRIMARY KEY (id);
ALTER TABLE ONLY app_public.users
    ADD CONSTRAINT users_pkey PRIMARY KEY (id);
ALTER TABLE ONLY app_public.users
    ADD CONSTRAINT users_username_key UNIQUE (username);
CREATE TRIGGER messages_announce AFTER INSERT OR UPDATE ON app_public.messages FOR EACH ROW EXECUTE FUNCTION app_public.tg_messages__announce();
ALTER TABLE ONLY app_public.messages
    ADD CONSTRAINT messages_author_id_fkey FOREIGN KEY (author_id) REFERENCES app_public.users(id);
ALTER TABLE ONLY app_public.messages
    ADD CONSTRAINT messages_forum_id_fkey FOREIGN KEY (forum_id) REFERENCES app_public.forums(id);
