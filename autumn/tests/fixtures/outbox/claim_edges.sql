-- Edge rows for the outbox claim (#3203). The Postgres and SQLite tests use
-- this file.
-- `__NOW__` is the relay clock in epoch ms. Topic `t` has a handler; `u` has
-- none. `payload` holds the expected result of one claim: `{"claim":true}` or
-- `{"claim":false}`. Rows get their `seq` in the order of this list.
INSERT INTO autumn_outbox
    (id, aggregate, topic, payload, created_at, available_at,
     claim_token, locked_until, dispatched_at, dead_at)
VALUES
    -- Ready.
    ('free', 'a1', 't', '{"claim":true}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL),
    -- Ready at the `available_at` bound.
    ('due-now', 'a2', 't', '{"claim":true}', __NOW__, __NOW__, NULL, NULL, NULL, NULL),
    -- In backoff for 1 ms more.
    ('due-later', 'a3', 't', '{"claim":false}', __NOW__, __NOW__ + 1, NULL, NULL, NULL, NULL),
    -- An older row of the aggregate is pending.
    ('behind-due-later', 'a3', 't', '{"claim":false}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL),
    -- The lease of a crashed relay ends now.
    ('lease-ends-now', 'a4', 't', '{"claim":true}', __NOW__, __NOW__ - 1, 'crashed', __NOW__, NULL, NULL),
    -- The lease holds for 1 ms more.
    ('lease-held', 'a5', 't', '{"claim":false}', __NOW__, __NOW__ - 1, 'live', __NOW__ + 1, NULL, NULL),
    ('behind-lease-held', 'a5', 't', '{"claim":false}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL),
    -- A sent row and a dead row are not claimed, and do not block.
    ('sent', 'a6', 't', '{"claim":false}', __NOW__, __NOW__ - 1, NULL, NULL, __NOW__ - 1, NULL),
    ('behind-sent', 'a6', 't', '{"claim":true}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL),
    ('dead', 'a7', 't', '{"claim":false}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, __NOW__ - 1),
    ('behind-dead', 'a7', 't', '{"claim":true}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL),
    -- Topic `u` has no handler. The pending row still blocks its aggregate.
    ('other-topic', 'a8', 'u', '{"claim":false}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL),
    ('behind-other-topic', 'a8', 't', '{"claim":false}', __NOW__, __NOW__ - 1, NULL, NULL, NULL, NULL);
