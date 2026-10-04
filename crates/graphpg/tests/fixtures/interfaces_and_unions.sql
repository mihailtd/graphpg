-- Interfaces and unions fixture (single-table, class-table and union polymorphism).
-- Adapted from the PostGraphile/Graphile Crystal test fixtures (MIT).
SET check_function_bodies = false;

CREATE SCHEMA interfaces_and_unions;
CREATE TYPE interfaces_and_unions.item_type AS ENUM (
    'TOPIC',
    'POST',
    'DIVIDER',
    'CHECKLIST',
    'CHECKLIST_ITEM'
);
CREATE TYPE interfaces_and_unions.union__entity AS (
	person_id integer,
	post_id integer,
	comment_id integer
);
COMMENT ON TYPE interfaces_and_unions.union__entity IS '@union Entity
@foreignKey (person_id) references interfaces_and_unions.people
@foreignKey (post_id) references interfaces_and_unions.posts
@foreignKey (comment_id) references interfaces_and_unions.comments
Exactly one of these should be not-null at a time';
CREATE TABLE interfaces_and_unions.relational_posts (
    id integer NOT NULL,
    title text NOT NULL,
    description text DEFAULT '-- Enter description here --'::text,
    note text
);
CREATE FUNCTION interfaces_and_unions.insert_post(author_id integer, title text) RETURNS interfaces_and_unions.relational_posts
    LANGUAGE sql
    AS $$
with item as (
  insert into interfaces_and_unions.relational_items (type, author_id)
  values ('POST', author_id)
  returning id
)
insert into interfaces_and_unions.relational_posts (id, title)
select item.id, insert_post.title from item
returning *
$$;
CREATE FUNCTION interfaces_and_unions.relational_posts_title_lower(p interfaces_and_unions.relational_posts) RETURNS text
    LANGUAGE sql STABLE
    AS $$
  select lower(p.title);
$$;
CREATE FUNCTION interfaces_and_unions.search(query text) RETURNS SETOF interfaces_and_unions.union__entity
    LANGUAGE sql STABLE
    AS $$
  select (person_id, null, null)::interfaces_and_unions.union__entity
  from interfaces_and_unions.people
  where username like '%' || query || '%'
union all
  select (null, post_id, null)::interfaces_and_unions.union__entity
  from interfaces_and_unions.posts
  where body like '%' || query || '%'
union all
  select (null, null, comment_id)::interfaces_and_unions.union__entity
  from interfaces_and_unions.comments
  where body like '%' || query || '%'
$$;
CREATE FUNCTION interfaces_and_unions.tg__relational_commentable() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
begin
  insert into interfaces_and_unions.relational_commentables (id, type)
    select id, TG_ARGV[0]::interfaces_and_unions.item_type
    from new_table;
  return NULL;
end;
$$;
CREATE FUNCTION interfaces_and_unions.tg__set_type2() RETURNS trigger
    LANGUAGE plpgsql
    AS $$
begin
  -- Cannot do this via `generated always as (type::text) stored` since casting
  -- an enum to text is not immutable (since you might rename the enum value
  -- later). We won't be doing that though; so work around it with a trigger.
  NEW.type2 = NEW.type::text;
  return NEW;
end;
$$;
CREATE TABLE interfaces_and_unions.aws_application_first_party_vulnerabilities (
    aws_application_id integer CONSTRAINT aws_application_first_party_vulnera_aws_application_id_not_null NOT NULL,
    first_party_vulnerability_id integer CONSTRAINT aws_application_first_party_first_party_vulnerability__not_null NOT NULL
);
CREATE TABLE interfaces_and_unions.aws_application_third_party_vulnerabilities (
    aws_application_id integer CONSTRAINT aws_application_third_party_vulnera_aws_application_id_not_null NOT NULL,
    third_party_vulnerability_id integer CONSTRAINT aws_application_third_party_third_party_vulnerability__not_null NOT NULL
);
CREATE TABLE interfaces_and_unions.aws_applications (
    id integer NOT NULL,
    name text NOT NULL,
    last_deployed timestamp with time zone,
    aws_id text
);
CREATE TABLE interfaces_and_unions.comments (
    comment_id integer NOT NULL,
    author_id integer NOT NULL,
    post_id integer NOT NULL,
    body text NOT NULL
);
CREATE SEQUENCE interfaces_and_unions.comments_comment_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.comments_comment_id_seq OWNED BY interfaces_and_unions.comments.comment_id;
CREATE TABLE interfaces_and_unions.enum_table_item_type (
    type text NOT NULL,
    description text
);
CREATE TABLE interfaces_and_unions.first_party_vulnerabilities (
    id integer NOT NULL,
    name text NOT NULL,
    cvss_score double precision NOT NULL,
    team_name text
);
CREATE TABLE interfaces_and_unions.gcp_application_first_party_vulnerabilities (
    gcp_application_id integer CONSTRAINT gcp_application_first_party_vulnera_gcp_application_id_not_null NOT NULL,
    first_party_vulnerability_id integer CONSTRAINT gcp_application_first_party_first_party_vulnerability__not_null NOT NULL
);
CREATE TABLE interfaces_and_unions.gcp_application_third_party_vulnerabilities (
    gcp_application_id integer CONSTRAINT gcp_application_third_party_vulnera_gcp_application_id_not_null NOT NULL,
    third_party_vulnerability_id integer CONSTRAINT gcp_application_third_party_third_party_vulnerability__not_null NOT NULL
);
CREATE TABLE interfaces_and_unions.gcp_applications (
    id integer NOT NULL,
    name text NOT NULL,
    last_deployed timestamp with time zone,
    gcp_id text
);
CREATE TABLE interfaces_and_unions.people (
    person_id integer NOT NULL,
    username text NOT NULL
);
CREATE SEQUENCE interfaces_and_unions.people_person_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.people_person_id_seq OWNED BY interfaces_and_unions.people.person_id;
CREATE TABLE interfaces_and_unions.person_bookmarks (
    id integer NOT NULL,
    person_id integer NOT NULL,
    bookmarked_entity interfaces_and_unions.union__entity NOT NULL
);
CREATE SEQUENCE interfaces_and_unions.person_bookmarks_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.person_bookmarks_id_seq OWNED BY interfaces_and_unions.person_bookmarks.id;
CREATE TABLE interfaces_and_unions.person_favourites (
    id integer NOT NULL,
    person_id integer NOT NULL,
    liked_person_id integer,
    liked_post_id integer,
    liked_comment_id integer
);
COMMENT ON TABLE interfaces_and_unions.person_favourites IS '@union PersonFavouriteEntity favourite_person_id,favourite_post_id,favourite_comment_id';
CREATE SEQUENCE interfaces_and_unions.person_favourites_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.person_favourites_id_seq OWNED BY interfaces_and_unions.person_favourites.id;
CREATE TABLE interfaces_and_unions.person_likes (
    id integer NOT NULL,
    person_id integer NOT NULL,
    liked_entity interfaces_and_unions.union__entity NOT NULL
);
CREATE SEQUENCE interfaces_and_unions.person_likes_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.person_likes_id_seq OWNED BY interfaces_and_unions.person_likes.id;
CREATE TABLE interfaces_and_unions.posts (
    post_id integer NOT NULL,
    author_id integer NOT NULL,
    body text NOT NULL
);
CREATE SEQUENCE interfaces_and_unions.posts_post_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.posts_post_id_seq OWNED BY interfaces_and_unions.posts.post_id;
CREATE TABLE interfaces_and_unions.relational_checklist_items (
    id integer NOT NULL,
    description text NOT NULL,
    note text
);
CREATE TABLE interfaces_and_unions.relational_checklists (
    id integer NOT NULL,
    title text NOT NULL
);
CREATE TABLE interfaces_and_unions.relational_commentables (
    id integer NOT NULL,
    type interfaces_and_unions.item_type NOT NULL,
    type2 text NOT NULL,
    CONSTRAINT relational_commentables_type_check CHECK ((type = ANY (ARRAY['POST'::interfaces_and_unions.item_type, 'CHECKLIST'::interfaces_and_unions.item_type, 'CHECKLIST_ITEM'::interfaces_and_unions.item_type])))
);
COMMENT ON TABLE interfaces_and_unions.relational_commentables IS '@interface relational type
@type POST relational_posts
@type CHECKLIST relational_checklists
@type CHECKLIST_ITEM relational_checklist_items';
CREATE SEQUENCE interfaces_and_unions.relational_commentables_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.relational_commentables_id_seq OWNED BY interfaces_and_unions.relational_commentables.id;
CREATE TABLE interfaces_and_unions.relational_dividers (
    id integer NOT NULL,
    title text,
    color text
);
CREATE TABLE interfaces_and_unions.relational_items (
    id integer NOT NULL,
    type interfaces_and_unions.item_type DEFAULT 'POST'::interfaces_and_unions.item_type NOT NULL,
    type2 text NOT NULL,
    parent_id integer,
    author_id integer NOT NULL,
    "position" bigint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    is_explicitly_archived boolean DEFAULT false NOT NULL,
    archived_at timestamp with time zone
);
COMMENT ON TABLE interfaces_and_unions.relational_items IS '@interface relational type
@type TOPIC relational_topics
@type POST relational_posts
@type DIVIDER relational_dividers
@type CHECKLIST relational_checklists
@type CHECKLIST_ITEM relational_checklist_items';
CREATE SEQUENCE interfaces_and_unions.relational_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.relational_items_id_seq OWNED BY interfaces_and_unions.relational_items.id;
CREATE TABLE interfaces_and_unions.relational_topics (
    id integer NOT NULL,
    title text NOT NULL
);
CREATE TABLE interfaces_and_unions.single_table_items (
    id integer NOT NULL,
    type interfaces_and_unions.item_type DEFAULT 'POST'::interfaces_and_unions.item_type NOT NULL,
    type2 text NOT NULL,
    parent_id integer,
    author_id integer NOT NULL,
    "position" bigint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    is_explicitly_archived boolean DEFAULT false NOT NULL,
    archived_at timestamp with time zone,
    title text,
    description text,
    note text,
    color text
);
COMMENT ON TABLE interfaces_and_unions.single_table_items IS '@interface single_table type
@type TOPIC title
@type POST title,description,note
@type DIVIDER title,color
@type CHECKLIST title
@type CHECKLIST_ITEM description,note';
CREATE SEQUENCE interfaces_and_unions.single_table_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.single_table_items_id_seq OWNED BY interfaces_and_unions.single_table_items.id;
CREATE TABLE interfaces_and_unions.third_party_vulnerabilities (
    id integer NOT NULL,
    name text NOT NULL,
    cvss_score double precision NOT NULL,
    vendor_name text
);
CREATE TABLE interfaces_and_unions.union_checklist_items (
    id integer NOT NULL,
    description text NOT NULL,
    note text
);
CREATE TABLE interfaces_and_unions.union_checklists (
    id integer NOT NULL,
    title text NOT NULL
);
CREATE TABLE interfaces_and_unions.union_dividers (
    id integer NOT NULL,
    title text,
    color text
);
CREATE TABLE interfaces_and_unions.union_items (
    id integer NOT NULL,
    type interfaces_and_unions.item_type DEFAULT 'POST'::interfaces_and_unions.item_type NOT NULL,
    type2 text NOT NULL
);
COMMENT ON TABLE interfaces_and_unions.union_items IS '@union relational type
@type TOPIC union_topics
@type POST union_posts
@type DIVIDER union_dividers
@type CHECKLIST union_checklists
@type CHECKLIST_ITEM union_checklist_items';
CREATE SEQUENCE interfaces_and_unions.union_items_id_seq
    AS integer
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;
ALTER SEQUENCE interfaces_and_unions.union_items_id_seq OWNED BY interfaces_and_unions.union_items.id;
CREATE TABLE interfaces_and_unions.union_posts (
    id integer NOT NULL,
    title text NOT NULL,
    description text,
    note text
);
CREATE TABLE interfaces_and_unions.union_topics (
    id integer NOT NULL,
    title text NOT NULL
);
ALTER TABLE ONLY interfaces_and_unions.comments ALTER COLUMN comment_id SET DEFAULT nextval('interfaces_and_unions.comments_comment_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.people ALTER COLUMN person_id SET DEFAULT nextval('interfaces_and_unions.people_person_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.person_bookmarks ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.person_bookmarks_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.person_favourites ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.person_favourites_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.person_likes ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.person_likes_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.posts ALTER COLUMN post_id SET DEFAULT nextval('interfaces_and_unions.posts_post_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.relational_commentables ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.relational_commentables_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.relational_items ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.relational_items_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.single_table_items ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.single_table_items_id_seq'::regclass);
ALTER TABLE ONLY interfaces_and_unions.union_items ALTER COLUMN id SET DEFAULT nextval('interfaces_and_unions.union_items_id_seq'::regclass);
INSERT INTO interfaces_and_unions.aws_application_first_party_vulnerabilities VALUES (1, 1);
INSERT INTO interfaces_and_unions.aws_application_first_party_vulnerabilities VALUES (1, 3);
INSERT INTO interfaces_and_unions.aws_application_first_party_vulnerabilities VALUES (1, 4);
INSERT INTO interfaces_and_unions.aws_application_first_party_vulnerabilities VALUES (1, 5);
INSERT INTO interfaces_and_unions.aws_application_third_party_vulnerabilities VALUES (1, 1);
INSERT INTO interfaces_and_unions.aws_application_third_party_vulnerabilities VALUES (1, 2);
INSERT INTO interfaces_and_unions.aws_application_third_party_vulnerabilities VALUES (1, 4);
INSERT INTO interfaces_and_unions.aws_application_third_party_vulnerabilities VALUES (1, 5);
INSERT INTO interfaces_and_unions.aws_applications VALUES (1, 'AWS App 1', NULL, 'AWS-0001');
INSERT INTO interfaces_and_unions.aws_applications VALUES (2, 'AWeSome', '2021-06-05 04:03:02.01+00', 'AWS-0002');
INSERT INTO interfaces_and_unions.comments VALUES (1, 2, 7, 'Yeah, that''s critical these days, right?');
INSERT INTO interfaces_and_unions.comments VALUES (2, 6, 7, 'Sucks, but it''s true.');
INSERT INTO interfaces_and_unions.comments VALUES (3, 3, 3, 'Can we have some vegan pizza for Sam?');
INSERT INTO interfaces_and_unions.comments VALUES (4, 5, 3, 'Of course; I''ll grab a selection. Dave: BYOB!');
INSERT INTO interfaces_and_unions.enum_table_item_type VALUES ('TOPIC', NULL);
INSERT INTO interfaces_and_unions.enum_table_item_type VALUES ('POST', NULL);
INSERT INTO interfaces_and_unions.enum_table_item_type VALUES ('DIVIDER', NULL);
INSERT INTO interfaces_and_unions.enum_table_item_type VALUES ('CHECKLIST', NULL);
INSERT INTO interfaces_and_unions.enum_table_item_type VALUES ('CHECKLIST_ITEM', NULL);
INSERT INTO interfaces_and_unions.first_party_vulnerabilities VALUES (1, 'Off-by-one', 3, 'Accounting');
INSERT INTO interfaces_and_unions.first_party_vulnerabilities VALUES (2, 'Index-out-of-bounds', 7.2, 'Retention');
INSERT INTO interfaces_and_unions.first_party_vulnerabilities VALUES (3, 'Exponential backtracking', 7.7, 'Continuity');
INSERT INTO interfaces_and_unions.first_party_vulnerabilities VALUES (4, 'Information disclosure', 7.2, 'Retention');
INSERT INTO interfaces_and_unions.first_party_vulnerabilities VALUES (5, 'Timing attack', 7.2, 'Retention');
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (1, 2);
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (2, 2);
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (2, 3);
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (1, 4);
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (1, 5);
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (2, 4);
INSERT INTO interfaces_and_unions.gcp_application_first_party_vulnerabilities VALUES (2, 5);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (1, 3);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (2, 1);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (2, 3);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (1, 4);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (1, 5);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (2, 4);
INSERT INTO interfaces_and_unions.gcp_application_third_party_vulnerabilities VALUES (2, 5);
INSERT INTO interfaces_and_unions.gcp_applications VALUES (1, 'GCP App 1', NULL, 'GCP_0_1');
INSERT INTO interfaces_and_unions.gcp_applications VALUES (2, 'Grand Crayon Pasta', '2022-10-10 10:10:10.101+00', 'GCP_0_2');
INSERT INTO interfaces_and_unions.people VALUES (1, 'Alice');
INSERT INTO interfaces_and_unions.people VALUES (2, 'Benjie');
INSERT INTO interfaces_and_unions.people VALUES (3, 'Caroline');
INSERT INTO interfaces_and_unions.people VALUES (4, 'Dave');
INSERT INTO interfaces_and_unions.people VALUES (5, 'Ellie');
INSERT INTO interfaces_and_unions.people VALUES (6, 'Fred');
INSERT INTO interfaces_and_unions.people VALUES (7, 'Georgina');
INSERT INTO interfaces_and_unions.people VALUES (8, 'Harry');
INSERT INTO interfaces_and_unions.person_bookmarks VALUES (1, 2, '(,3,)');
INSERT INTO interfaces_and_unions.person_bookmarks VALUES (2, 2, '(,,1)');
INSERT INTO interfaces_and_unions.person_bookmarks VALUES (3, 5, '(3,,)');
INSERT INTO interfaces_and_unions.person_favourites VALUES (1, 2, 3, NULL, NULL);
INSERT INTO interfaces_and_unions.person_favourites VALUES (2, 2, 4, NULL, NULL);
INSERT INTO interfaces_and_unions.person_favourites VALUES (3, 2, 5, NULL, NULL);
INSERT INTO interfaces_and_unions.person_favourites VALUES (4, 2, NULL, 5, NULL);
INSERT INTO interfaces_and_unions.person_favourites VALUES (5, 2, NULL, 6, NULL);
INSERT INTO interfaces_and_unions.person_favourites VALUES (6, 2, NULL, 7, NULL);
INSERT INTO interfaces_and_unions.person_favourites VALUES (7, 2, NULL, NULL, 2);
INSERT INTO interfaces_and_unions.person_favourites VALUES (8, 2, NULL, NULL, 3);
INSERT INTO interfaces_and_unions.person_likes VALUES (1, 2, '(3,,)');
INSERT INTO interfaces_and_unions.person_likes VALUES (2, 2, '(4,,)');
INSERT INTO interfaces_and_unions.person_likes VALUES (3, 2, '(5,,)');
INSERT INTO interfaces_and_unions.person_likes VALUES (4, 2, '(,5,)');
INSERT INTO interfaces_and_unions.person_likes VALUES (5, 2, '(,6,)');
INSERT INTO interfaces_and_unions.person_likes VALUES (6, 2, '(,7,)');
INSERT INTO interfaces_and_unions.person_likes VALUES (7, 2, '(,,2)');
INSERT INTO interfaces_and_unions.person_likes VALUES (8, 2, '(,,3)');
INSERT INTO interfaces_and_unions.posts VALUES (1, 5, 'Dave do you fancy pizza?');
INSERT INTO interfaces_and_unions.posts VALUES (2, 4, 'Of course, I love pizza!');
INSERT INTO interfaces_and_unions.posts VALUES (3, 5, 'Sweet, I''ll order some Mighty Meaty');
INSERT INTO interfaces_and_unions.posts VALUES (4, 2, 'Sounds delicious; I''m in!');
INSERT INTO interfaces_and_unions.posts VALUES (5, 3, 'Let''s make a party of it!');
INSERT INTO interfaces_and_unions.posts VALUES (6, 3, 'Come round ours, we can hang out on the new patio');
INSERT INTO interfaces_and_unions.posts VALUES (7, 6, 'Socially distanced pizzas? Count us in too!');
INSERT INTO interfaces_and_unions.relational_checklist_items VALUES (17, 'Follows pattern of GraphQL resolver flow', NULL);
INSERT INTO interfaces_and_unions.relational_checklist_items VALUES (18, 'Has an optimisation phase', NULL);
INSERT INTO interfaces_and_unions.relational_checklist_items VALUES (19, 'Plan deduplication at the field level', NULL);
INSERT INTO interfaces_and_unions.relational_checklist_items VALUES (20, 'Garbage-collection of unused plans', NULL);
INSERT INTO interfaces_and_unions.relational_checklist_items VALUES (21, 'Supports newest GraphQL features', NULL);
INSERT INTO interfaces_and_unions.relational_checklists VALUES (16, 'Planning goals');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (6, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (5, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (17, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (20, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (19, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (9, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (13, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (21, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (18, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (4, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (15, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (7, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (16, 'CHECKLIST', 'CHECKLIST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (14, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_commentables VALUES (12, 'POST', 'POST');
INSERT INTO interfaces_and_unions.relational_dividers VALUES (3, 'Headline features', 'green');
INSERT INTO interfaces_and_unions.relational_dividers VALUES (8, 'Timescale', 'blue');
INSERT INTO interfaces_and_unions.relational_items VALUES (1, 'TOPIC', 'TOPIC', NULL, 2, 0, '2020-01-28 11:00:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (2, 'TOPIC', 'TOPIC', NULL, 1, 0, '2020-03-26 13:00:00+00', '2020-03-26 14:00:00+00', true, '2020-03-26 14:00:00+00');
INSERT INTO interfaces_and_unions.relational_items VALUES (3, 'DIVIDER', 'DIVIDER', 1, 2, 0, '2020-01-28 11:00:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (4, 'POST', 'POST', 1, 2, 1, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (5, 'POST', 'POST', 1, 2, 2, '2020-01-28 11:02:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (6, 'POST', 'POST', 1, 2, 3, '2020-01-28 11:03:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (7, 'POST', 'POST', 1, 2, 4, '2020-01-28 11:04:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (8, 'DIVIDER', 'DIVIDER', 1, 2, 5, '2020-01-28 11:05:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (9, 'POST', 'POST', 1, 2, 6, '2020-01-28 11:06:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (10, 'TOPIC', 'TOPIC', 1, 2, 7, '2020-01-28 11:07:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (11, 'TOPIC', 'TOPIC', 10, 2, 0, '2020-01-28 11:08:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (12, 'POST', 'POST', 11, 2, 0, '2020-01-28 11:09:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (13, 'POST', 'POST', 11, 2, 1, '2020-01-28 11:10:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (14, 'POST', 'POST', 11, 2, 2, '2020-01-28 11:11:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (15, 'POST', 'POST', 2, 1, 0, '2020-01-28 11:11:00+00', '2021-07-30 14:24:00+00', false, '2020-03-26 14:00:00+00');
INSERT INTO interfaces_and_unions.relational_items VALUES (16, 'CHECKLIST', 'CHECKLIST', 4, 2, 0, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (17, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 2, 0, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (18, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 3, 1, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (19, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 2, 2, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (20, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 2, 3, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_items VALUES (21, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 1, 4, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (4, 'Better planning', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (5, 'Easier to code', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (6, 'More features', 'E.g. interfaces and unions', 'Also things like querying from multiple databases');
INSERT INTO interfaces_and_unions.relational_posts VALUES (7, 'Better performance', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (9, 'When have I ever committed to a timescale?', ':D', 'It''ll be done when it''s done, I prefer longer development time and longer stable time than multiple major releases in a year or two.');
INSERT INTO interfaces_and_unions.relational_posts VALUES (12, 'Fix legacy issues', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (13, 'Full TypeScript conversion', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (14, 'Monorepo', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_posts VALUES (15, 'Just a test', NULL, NULL);
INSERT INTO interfaces_and_unions.relational_topics VALUES (1, 'PostGraphile version 5');
INSERT INTO interfaces_and_unions.relational_topics VALUES (2, 'Temporary test topic');
INSERT INTO interfaces_and_unions.relational_topics VALUES (10, 'Notes');
INSERT INTO interfaces_and_unions.relational_topics VALUES (11, 'Other aims');
INSERT INTO interfaces_and_unions.single_table_items VALUES (1, 'TOPIC', 'TOPIC', NULL, 2, 0, '2020-01-28 11:00:00+00', '2021-07-30 14:24:00+00', false, NULL, 'PostGraphile version 5', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (2, 'TOPIC', 'TOPIC', NULL, 1, 0, '2020-03-26 13:00:00+00', '2020-03-26 14:00:00+00', true, '2020-03-26 14:00:00+00', 'Temporary test topic', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (3, 'DIVIDER', 'DIVIDER', 1, 2, 0, '2020-01-28 11:00:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Headline features', NULL, NULL, 'green');
INSERT INTO interfaces_and_unions.single_table_items VALUES (4, 'POST', 'POST', 1, 2, 1, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Better planning', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (5, 'POST', 'POST', 1, 2, 2, '2020-01-28 11:02:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Easier to code', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (6, 'POST', 'POST', 1, 2, 3, '2020-01-28 11:03:00+00', '2021-07-30 14:24:00+00', false, NULL, 'More features', 'E.g. interfaces and unions', 'Also things like querying from multiple databases', NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (7, 'POST', 'POST', 1, 2, 4, '2020-01-28 11:04:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Better performance', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (8, 'DIVIDER', 'DIVIDER', 1, 2, 5, '2020-01-28 11:05:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Timescale', NULL, NULL, 'blue');
INSERT INTO interfaces_and_unions.single_table_items VALUES (9, 'POST', 'POST', 1, 2, 6, '2020-01-28 11:06:00+00', '2021-07-30 14:24:00+00', false, NULL, 'When have I ever committed to a timescale?', ':D', 'It''ll be done when it''s done, I prefer longer development time and longer stable time than multiple major releases in a year or two.', NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (10, 'TOPIC', 'TOPIC', 1, 2, 7, '2020-01-28 11:07:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Notes', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (11, 'TOPIC', 'TOPIC', 10, 2, 0, '2020-01-28 11:08:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Other aims', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (12, 'POST', 'POST', 11, 2, 0, '2020-01-28 11:09:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Fix legacy issues', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (13, 'POST', 'POST', 11, 2, 1, '2020-01-28 11:10:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Full TypeScript conversion', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (14, 'POST', 'POST', 11, 2, 2, '2020-01-28 11:11:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Monorepo', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (15, 'POST', 'POST', 2, 1, 0, '2020-01-28 11:11:00+00', '2021-07-30 14:24:00+00', false, '2020-03-26 14:00:00+00', 'Just a test', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (16, 'CHECKLIST', 'CHECKLIST', 4, 2, 0, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, 'Planning goals', NULL, NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (17, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 2, 0, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, NULL, 'Follows pattern of GraphQL resolver flow', NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (18, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 3, 1, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, NULL, 'Has an optimisation phase', NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (19, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 2, 2, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, NULL, 'Plan deduplication at the field level', NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (20, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 2, 3, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, NULL, 'Garbage-collection of unused plans', NULL, NULL);
INSERT INTO interfaces_and_unions.single_table_items VALUES (21, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM', 16, 1, 4, '2020-01-28 11:01:00+00', '2021-07-30 14:24:00+00', false, NULL, NULL, 'Supports newest GraphQL features', NULL, NULL);
INSERT INTO interfaces_and_unions.third_party_vulnerabilities VALUES (1, 'CSRF', 7.5, '98-Factor-Login');
INSERT INTO interfaces_and_unions.third_party_vulnerabilities VALUES (2, 'XSS', 9.1, 'Frog-Render-Lib');
INSERT INTO interfaces_and_unions.third_party_vulnerabilities VALUES (3, 'SQL injection', 10, 'Eval-Sequel-Corp');
INSERT INTO interfaces_and_unions.third_party_vulnerabilities VALUES (4, 'Malware', 7.2, 'Frog-Render-Lib');
INSERT INTO interfaces_and_unions.third_party_vulnerabilities VALUES (5, 'License', 7.2, 'Frog-Render-Lib');
INSERT INTO interfaces_and_unions.union_checklist_items VALUES (17, 'Follows pattern of GraphQL resolver flow', NULL);
INSERT INTO interfaces_and_unions.union_checklist_items VALUES (18, 'Has an optimisation phase', NULL);
INSERT INTO interfaces_and_unions.union_checklist_items VALUES (19, 'Plan deduplication at the field level', NULL);
INSERT INTO interfaces_and_unions.union_checklist_items VALUES (20, 'Garbage-collection of unused plans', NULL);
INSERT INTO interfaces_and_unions.union_checklist_items VALUES (21, 'Supports newest GraphQL features', NULL);
INSERT INTO interfaces_and_unions.union_checklists VALUES (16, 'Planning goals');
INSERT INTO interfaces_and_unions.union_dividers VALUES (3, 'Headline features', 'green');
INSERT INTO interfaces_and_unions.union_dividers VALUES (8, 'Timescale', 'blue');
INSERT INTO interfaces_and_unions.union_items VALUES (1, 'TOPIC', 'TOPIC');
INSERT INTO interfaces_and_unions.union_items VALUES (2, 'TOPIC', 'TOPIC');
INSERT INTO interfaces_and_unions.union_items VALUES (3, 'DIVIDER', 'DIVIDER');
INSERT INTO interfaces_and_unions.union_items VALUES (4, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (5, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (6, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (7, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (8, 'DIVIDER', 'DIVIDER');
INSERT INTO interfaces_and_unions.union_items VALUES (9, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (10, 'TOPIC', 'TOPIC');
INSERT INTO interfaces_and_unions.union_items VALUES (11, 'TOPIC', 'TOPIC');
INSERT INTO interfaces_and_unions.union_items VALUES (12, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (13, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (14, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (15, 'POST', 'POST');
INSERT INTO interfaces_and_unions.union_items VALUES (16, 'CHECKLIST', 'CHECKLIST');
INSERT INTO interfaces_and_unions.union_items VALUES (17, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.union_items VALUES (18, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.union_items VALUES (19, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.union_items VALUES (20, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.union_items VALUES (21, 'CHECKLIST_ITEM', 'CHECKLIST_ITEM');
INSERT INTO interfaces_and_unions.union_posts VALUES (4, 'Better planning', NULL, NULL);
INSERT INTO interfaces_and_unions.union_posts VALUES (5, 'Easier to code', NULL, NULL);
INSERT INTO interfaces_and_unions.union_posts VALUES (6, 'More features', 'E.g. interfaces and unions', 'Also things like querying from multiple databases');
INSERT INTO interfaces_and_unions.union_posts VALUES (7, 'Better performance', NULL, NULL);
INSERT INTO interfaces_and_unions.union_posts VALUES (9, 'When have I ever committed to a timescale?', ':D', 'It''ll be done when it''s done, I prefer longer development time and longer stable time than multiple major releases in a year or two.');
INSERT INTO interfaces_and_unions.union_posts VALUES (12, 'Fix legacy issues', NULL, NULL);
INSERT INTO interfaces_and_unions.union_posts VALUES (13, 'Full TypeScript conversion', NULL, NULL);
INSERT INTO interfaces_and_unions.union_posts VALUES (14, 'Monorepo', NULL, NULL);
INSERT INTO interfaces_and_unions.union_posts VALUES (15, 'Just a test', NULL, NULL);
INSERT INTO interfaces_and_unions.union_topics VALUES (1, 'PostGraphile version 5');
INSERT INTO interfaces_and_unions.union_topics VALUES (2, 'Temporary test topic');
INSERT INTO interfaces_and_unions.union_topics VALUES (10, 'Notes');
INSERT INTO interfaces_and_unions.union_topics VALUES (11, 'Other aims');
ALTER TABLE ONLY interfaces_and_unions.aws_application_first_party_vulnerabilities
    ADD CONSTRAINT aws_application_first_party_vulnerabilities_pkey PRIMARY KEY (aws_application_id, first_party_vulnerability_id);
ALTER TABLE ONLY interfaces_and_unions.aws_application_third_party_vulnerabilities
    ADD CONSTRAINT aws_application_third_party_vulnerabilities_pkey PRIMARY KEY (aws_application_id, third_party_vulnerability_id);
ALTER TABLE ONLY interfaces_and_unions.aws_applications
    ADD CONSTRAINT aws_applications_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.comments
    ADD CONSTRAINT comments_pkey PRIMARY KEY (comment_id);
ALTER TABLE ONLY interfaces_and_unions.enum_table_item_type
    ADD CONSTRAINT enum_table_item_type_pkey PRIMARY KEY (type);
ALTER TABLE ONLY interfaces_and_unions.first_party_vulnerabilities
    ADD CONSTRAINT first_party_vulnerabilities_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.gcp_application_first_party_vulnerabilities
    ADD CONSTRAINT gcp_application_first_party_vulnerabilities_pkey PRIMARY KEY (gcp_application_id, first_party_vulnerability_id);
ALTER TABLE ONLY interfaces_and_unions.gcp_application_third_party_vulnerabilities
    ADD CONSTRAINT gcp_application_third_party_vulnerabilities_pkey PRIMARY KEY (gcp_application_id, third_party_vulnerability_id);
ALTER TABLE ONLY interfaces_and_unions.gcp_applications
    ADD CONSTRAINT gcp_applications_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.people
    ADD CONSTRAINT people_pkey PRIMARY KEY (person_id);
ALTER TABLE ONLY interfaces_and_unions.people
    ADD CONSTRAINT people_username_key UNIQUE (username);
ALTER TABLE ONLY interfaces_and_unions.person_bookmarks
    ADD CONSTRAINT person_bookmarks_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.person_favourites
    ADD CONSTRAINT person_favourites_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.person_likes
    ADD CONSTRAINT person_likes_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.posts
    ADD CONSTRAINT posts_pkey PRIMARY KEY (post_id);
ALTER TABLE ONLY interfaces_and_unions.relational_checklist_items
    ADD CONSTRAINT relational_checklist_items_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.relational_checklists
    ADD CONSTRAINT relational_checklists_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.relational_commentables
    ADD CONSTRAINT relational_commentables_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.relational_dividers
    ADD CONSTRAINT relational_dividers_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.relational_items
    ADD CONSTRAINT relational_items_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.relational_posts
    ADD CONSTRAINT relational_posts_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.relational_topics
    ADD CONSTRAINT relational_topics_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.single_table_items
    ADD CONSTRAINT single_table_items_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.third_party_vulnerabilities
    ADD CONSTRAINT third_party_vulnerabilities_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.union_checklist_items
    ADD CONSTRAINT union_checklist_items_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.union_checklists
    ADD CONSTRAINT union_checklists_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.union_dividers
    ADD CONSTRAINT union_dividers_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.union_items
    ADD CONSTRAINT union_items_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.union_posts
    ADD CONSTRAINT union_posts_pkey PRIMARY KEY (id);
ALTER TABLE ONLY interfaces_and_unions.union_topics
    ADD CONSTRAINT union_topics_pkey PRIMARY KEY (id);
CREATE INDEX aws_application_first_party_vu_first_party_vulnerability_id_idx ON interfaces_and_unions.aws_application_first_party_vulnerabilities USING btree (first_party_vulnerability_id);
CREATE INDEX aws_application_third_party_vu_third_party_vulnerability_id_idx ON interfaces_and_unions.aws_application_third_party_vulnerabilities USING btree (third_party_vulnerability_id);
CREATE INDEX gcp_application_first_party_vu_first_party_vulnerability_id_idx ON interfaces_and_unions.gcp_application_first_party_vulnerabilities USING btree (first_party_vulnerability_id);
CREATE INDEX gcp_application_third_party_vu_third_party_vulnerability_id_idx ON interfaces_and_unions.gcp_application_third_party_vulnerabilities USING btree (third_party_vulnerability_id);
CREATE TRIGGER _500_commentable AFTER INSERT ON interfaces_and_unions.relational_checklist_items REFERENCING NEW TABLE AS new_table FOR EACH STATEMENT EXECUTE FUNCTION interfaces_and_unions.tg__relational_commentable('CHECKLIST_ITEM');
CREATE TRIGGER _500_commentable AFTER INSERT ON interfaces_and_unions.relational_checklists REFERENCING NEW TABLE AS new_table FOR EACH STATEMENT EXECUTE FUNCTION interfaces_and_unions.tg__relational_commentable('CHECKLIST');
CREATE TRIGGER _500_commentable AFTER INSERT ON interfaces_and_unions.relational_posts REFERENCING NEW TABLE AS new_table FOR EACH STATEMENT EXECUTE FUNCTION interfaces_and_unions.tg__relational_commentable('POST');
CREATE TRIGGER set_type2 BEFORE INSERT OR UPDATE ON interfaces_and_unions.relational_commentables FOR EACH ROW EXECUTE FUNCTION interfaces_and_unions.tg__set_type2();
CREATE TRIGGER set_type2 BEFORE INSERT OR UPDATE ON interfaces_and_unions.relational_items FOR EACH ROW EXECUTE FUNCTION interfaces_and_unions.tg__set_type2();
CREATE TRIGGER set_type2 BEFORE INSERT OR UPDATE ON interfaces_and_unions.single_table_items FOR EACH ROW EXECUTE FUNCTION interfaces_and_unions.tg__set_type2();
CREATE TRIGGER set_type2 BEFORE INSERT OR UPDATE ON interfaces_and_unions.union_items FOR EACH ROW EXECUTE FUNCTION interfaces_and_unions.tg__set_type2();
ALTER TABLE ONLY interfaces_and_unions.aws_application_first_party_vulnerabilities
    ADD CONSTRAINT aws_application_first_party_v_first_party_vulnerability_id_fkey FOREIGN KEY (first_party_vulnerability_id) REFERENCES interfaces_and_unions.first_party_vulnerabilities(id);
ALTER TABLE ONLY interfaces_and_unions.aws_application_first_party_vulnerabilities
    ADD CONSTRAINT aws_application_first_party_vulnerabili_aws_application_id_fkey FOREIGN KEY (aws_application_id) REFERENCES interfaces_and_unions.aws_applications(id);
ALTER TABLE ONLY interfaces_and_unions.aws_application_third_party_vulnerabilities
    ADD CONSTRAINT aws_application_third_party_v_third_party_vulnerability_id_fkey FOREIGN KEY (third_party_vulnerability_id) REFERENCES interfaces_and_unions.third_party_vulnerabilities(id);
ALTER TABLE ONLY interfaces_and_unions.aws_application_third_party_vulnerabilities
    ADD CONSTRAINT aws_application_third_party_vulnerabili_aws_application_id_fkey FOREIGN KEY (aws_application_id) REFERENCES interfaces_and_unions.aws_applications(id);
ALTER TABLE ONLY interfaces_and_unions.comments
    ADD CONSTRAINT comments_author_id_fkey FOREIGN KEY (author_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.comments
    ADD CONSTRAINT comments_post_id_fkey FOREIGN KEY (post_id) REFERENCES interfaces_and_unions.posts(post_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.gcp_application_first_party_vulnerabilities
    ADD CONSTRAINT gcp_application_first_party_v_first_party_vulnerability_id_fkey FOREIGN KEY (first_party_vulnerability_id) REFERENCES interfaces_and_unions.first_party_vulnerabilities(id);
ALTER TABLE ONLY interfaces_and_unions.gcp_application_first_party_vulnerabilities
    ADD CONSTRAINT gcp_application_first_party_vulnerabili_gcp_application_id_fkey FOREIGN KEY (gcp_application_id) REFERENCES interfaces_and_unions.gcp_applications(id);
ALTER TABLE ONLY interfaces_and_unions.gcp_application_third_party_vulnerabilities
    ADD CONSTRAINT gcp_application_third_party_v_third_party_vulnerability_id_fkey FOREIGN KEY (third_party_vulnerability_id) REFERENCES interfaces_and_unions.third_party_vulnerabilities(id);
ALTER TABLE ONLY interfaces_and_unions.gcp_application_third_party_vulnerabilities
    ADD CONSTRAINT gcp_application_third_party_vulnerabili_gcp_application_id_fkey FOREIGN KEY (gcp_application_id) REFERENCES interfaces_and_unions.gcp_applications(id);
ALTER TABLE ONLY interfaces_and_unions.person_bookmarks
    ADD CONSTRAINT person_bookmarks_person_id_fkey FOREIGN KEY (person_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.person_favourites
    ADD CONSTRAINT person_favourites_liked_comment_id_fkey FOREIGN KEY (liked_comment_id) REFERENCES interfaces_and_unions.comments(comment_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.person_favourites
    ADD CONSTRAINT person_favourites_liked_person_id_fkey FOREIGN KEY (liked_person_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.person_favourites
    ADD CONSTRAINT person_favourites_liked_post_id_fkey FOREIGN KEY (liked_post_id) REFERENCES interfaces_and_unions.posts(post_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.person_favourites
    ADD CONSTRAINT person_favourites_person_id_fkey FOREIGN KEY (person_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.person_likes
    ADD CONSTRAINT person_likes_person_id_fkey FOREIGN KEY (person_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.posts
    ADD CONSTRAINT posts_author_id_fkey FOREIGN KEY (author_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.relational_checklist_items
    ADD CONSTRAINT relational_checklist_items_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_items(id);
ALTER TABLE ONLY interfaces_and_unions.relational_checklists
    ADD CONSTRAINT relational_checklists_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_items(id);
ALTER TABLE ONLY interfaces_and_unions.relational_commentables
    ADD CONSTRAINT relational_commentables_type2_fkey FOREIGN KEY (type2) REFERENCES interfaces_and_unions.enum_table_item_type(type);
ALTER TABLE ONLY interfaces_and_unions.relational_dividers
    ADD CONSTRAINT relational_dividers_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_items(id);
ALTER TABLE ONLY interfaces_and_unions.relational_items
    ADD CONSTRAINT relational_items_author_id_fkey FOREIGN KEY (author_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.relational_items
    ADD CONSTRAINT relational_items_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES interfaces_and_unions.relational_items(id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.relational_items
    ADD CONSTRAINT relational_items_type2_fkey FOREIGN KEY (type2) REFERENCES interfaces_and_unions.enum_table_item_type(type);
ALTER TABLE ONLY interfaces_and_unions.relational_checklist_items
    ADD CONSTRAINT relational_posts_commentable_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_commentables(id) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE ONLY interfaces_and_unions.relational_checklists
    ADD CONSTRAINT relational_posts_commentable_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_commentables(id) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE ONLY interfaces_and_unions.relational_posts
    ADD CONSTRAINT relational_posts_commentable_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_commentables(id) DEFERRABLE INITIALLY DEFERRED;
ALTER TABLE ONLY interfaces_and_unions.relational_posts
    ADD CONSTRAINT relational_posts_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_items(id);
ALTER TABLE ONLY interfaces_and_unions.relational_topics
    ADD CONSTRAINT relational_topics_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.relational_items(id);
ALTER TABLE ONLY interfaces_and_unions.single_table_items
    ADD CONSTRAINT single_table_items_author_id_fkey FOREIGN KEY (author_id) REFERENCES interfaces_and_unions.people(person_id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.single_table_items
    ADD CONSTRAINT single_table_items_parent_id_fkey FOREIGN KEY (parent_id) REFERENCES interfaces_and_unions.single_table_items(id) ON DELETE CASCADE;
ALTER TABLE ONLY interfaces_and_unions.single_table_items
    ADD CONSTRAINT single_table_items_type2_fkey FOREIGN KEY (type2) REFERENCES interfaces_and_unions.enum_table_item_type(type);
ALTER TABLE ONLY interfaces_and_unions.union_checklist_items
    ADD CONSTRAINT union_checklist_items_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.union_items(id);
ALTER TABLE ONLY interfaces_and_unions.union_checklists
    ADD CONSTRAINT union_checklists_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.union_items(id);
ALTER TABLE ONLY interfaces_and_unions.union_dividers
    ADD CONSTRAINT union_dividers_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.union_items(id);
ALTER TABLE ONLY interfaces_and_unions.union_items
    ADD CONSTRAINT union_items_type2_fkey FOREIGN KEY (type2) REFERENCES interfaces_and_unions.enum_table_item_type(type);
ALTER TABLE ONLY interfaces_and_unions.union_posts
    ADD CONSTRAINT union_posts_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.union_items(id);
ALTER TABLE ONLY interfaces_and_unions.union_topics
    ADD CONSTRAINT union_topics_id_fkey FOREIGN KEY (id) REFERENCES interfaces_and_unions.union_items(id);
