"""Emit SQL that defines pg_temp functions running the page read of
statements.sql (tag `old`) and statements-after.sql (tag `new`).

usage: python3 equivalence_sql.py statements.sql statements-after.sql
"""
import re
import sys

DIGEST = (
    "SELECT count(*) FILTER (WHERE depth = 0), count(*), "
    "md5(coalesce(string_agg(concat_ws('|', id, parent_id, author_id, md5(body), "
    "created_at, author_name, depth), ',' ORDER BY ord), '')), "
    "(array_agg(id ORDER BY ord DESC) FILTER (WHERE depth = 0))[1] "
    "FROM (SELECT x.*, row_number() OVER () ord FROM (%s) x) y"
)


def stmts(path):
    src = open(path).read()
    return dict(re.findall(r"PREPARE (\w+)\([^)]*\) AS\n(.*?);\n", src, re.S))


def lit(text):
    return "$q$" + text + "$q$"


for tag, path in (("old", sys.argv[1]), ("new", sys.argv[2])):
    st = stmts(path)
    first = lit(DIGEST % st["page_first"])
    after = lit(DIGEST % st["page_after"])
    print(f"""
CREATE FUNCTION pg_temp.walk_{tag}(post bigint, lim bigint)
RETURNS TABLE(page int, top int, n int, digest text) LANGUAGE plpgsql AS $f$
DECLARE cur bigint; p int := 1; nxt bigint; t int; c int; h text; last bigint;
BEGIN
  LOOP
    IF cur IS NULL THEN
      EXECUTE {first} INTO t, c, h, last USING 'Post', post, lim, 'Post', post, post;
    ELSE
      EXECUTE {after} INTO t, c, h, last USING 'Post', post, cur, 'Post', post, lim, 'Post', post, post;
    END IF;
    page := p; top := t; n := c; digest := h; RETURN NEXT;
    EXIT WHEN t < lim OR last IS NULL;
    EXECUTE {lit(st["next_top"])} INTO nxt USING 'Post', post, last, 'Post', post;
    EXIT WHEN nxt IS NULL;
    cur := last; p := p + 1;
  END LOOP;
END $f$;

CREATE FUNCTION pg_temp.after_{tag}(post bigint, cur bigint, lim bigint)
RETURNS text LANGUAGE plpgsql AS $f$
DECLARE t int; c int; h text; last bigint;
BEGIN
  EXECUTE {after} INTO t, c, h, last USING 'Post', post, cur, 'Post', post, lim, 'Post', post, post;
  RETURN c || ' rows ' || h;
END $f$;

CREATE FUNCTION pg_temp.rows_{tag}(post bigint)
RETURNS TABLE(id bigint, parent_id bigint, author_id bigint, body text,
              created_at timestamp, author_name text, depth bigint)
LANGUAGE plpgsql AS $f$
BEGIN
  RETURN QUERY EXECUTE {lit(st["page_first"])} USING 'Post', post, 50::bigint, 'Post', post, post;
END $f$;""")
