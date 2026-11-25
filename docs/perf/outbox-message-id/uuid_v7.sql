-- uuid_v7(): a version-7 UUID as text, the layout Uuid::now_v7 makes: the
-- Unix time in milliseconds in the first 48 bits, then the version nibble 7,
-- the RFC 4122 variant, and random bits. Postgres 16 has no uuidv7() (18
-- does). The harness only; never shipped.
CREATE OR REPLACE FUNCTION uuid_v7() RETURNS text LANGUAGE sql VOLATILE AS $$
  SELECT encode(set_byte(v, 6, (get_byte(v, 6) & 15) | 112), 'hex')::uuid::text
    FROM (SELECT overlay(uuid_send(gen_random_uuid())
                 PLACING substring(int8send(floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint) FROM 3)
                 FROM 1 FOR 6) AS v) AS b
$$;
