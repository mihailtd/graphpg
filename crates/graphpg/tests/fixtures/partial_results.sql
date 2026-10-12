-- Root fields that fail on their own, for partial results (tests/partial_results.rs).
CREATE SCHEMA partial_results;

CREATE TABLE partial_results.items (
    id integer PRIMARY KEY,
    name text NOT NULL
);
INSERT INTO partial_results.items VALUES (1, 'one'), (2, 'two');

-- A computed column that fails for the row named 'two'
CREATE FUNCTION partial_results.items_checked(item partial_results.items) RETURNS text
LANGUAGE plpgsql STABLE AS $$
BEGIN
    IF item.name = 'two' THEN
        RAISE EXCEPTION 'item two cannot be checked';
    END IF;
    RETURN 'ok';
END $$;

-- A nullable root field that always fails
CREATE FUNCTION partial_results.broken() RETURNS integer
LANGUAGE plpgsql STABLE AS $$
BEGIN
    RETURN 1 / 0;
END $$;

-- A root field slower than the tests' statement timeout
CREATE FUNCTION partial_results.slow() RETURNS integer
LANGUAGE sql STABLE AS $$ SELECT 1 FROM pg_sleep(2) $$;
