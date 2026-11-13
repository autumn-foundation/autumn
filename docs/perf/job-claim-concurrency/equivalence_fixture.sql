-- Edge cases for the claim's concurrency filter. Small on purpose: every row matters.
TRUNCATE autumn_jobs;
INSERT INTO autumn_jobs (id, name, status, run_at, queue, concurrency_key, concurrency_limit) VALUES
 -- NULL key vs '' vs the string 'null': three distinct groups, limit 1 each
 ('k_null_1', 'grp', 'enqueued', now() - interval '10 min', 'default', NULL, 1),
 ('k_null_2', 'grp', 'enqueued', now() - interval '9 min',  'default', NULL, 1),
 ('k_empty_1','grp', 'enqueued', now() - interval '10 min', 'default', '', 1),
 ('k_empty_2','grp', 'enqueued', now() - interval '8 min',  'default', '', 1),
 ('k_str_1',  'grp', 'enqueued', now() - interval '10 min', 'default', 'null', 1),
 ('k_str_2',  'grp', 'enqueued', now() - interval '7 min',  'default', 'null', 1),
 -- mixed limits inside one group ("m"): limit 1 rows block once one runs, limit 3 rows do not
 ('m_l1_a', 'mix', 'enqueued', now() - interval '6 min', 'default', 'm', 1),
 ('m_l1_b', 'mix', 'enqueued', now() - interval '5 min', 'default', 'm', 1),
 ('m_l3_a', 'mix', 'enqueued', now() - interval '4 min', 'default', 'm', 3),
 ('m_l3_b', 'mix', 'enqueued', now() - interval '3 min', 'default', 'm', 3),
 ('m_l3_c', 'mix', 'enqueued', now() - interval '2 min', 'default', 'm', 3),
 -- limit 0: never claimable
 ('zero', 'z', 'enqueued', now() - interval '20 min', 'default', 'k', 0),
 -- a group already saturated by a running row that itself has no limit
 ('sat_q', 'sat', 'enqueued', now() - interval '30 min', 'default', 's', 1),
 ('sat_run', 'sat', 'running', now() - interval '1 hour', 'default', 's', NULL),
 -- same name and key in another queue: its running row still counts (count is not per queue)
 ('xq_run', 'xq', 'running', now() - interval '1 hour', 'other', 'x', NULL),
 ('xq_q',   'xq', 'enqueued', now() - interval '25 min', 'default', 'x', 1),
 -- names and keys that need JSON escaping
 ('esc_1', E'we"ird\\naéme', 'enqueued', now() - interval '15 min', 'default', E'k"\\', 1),
 ('esc_2', E'we"ird\\naéme', 'enqueued', now() - interval '14 min', 'default', E'k"\\', 1),
 ('esc_3', E'we"ird\\naéme', 'enqueued', now() - interval '13 min', 'default', E'k"', 1),
 -- unlimited rows, a run_at tie, the now() boundary, a future row, another queue, a deferred name
 ('u_tie_a', 'plain', 'enqueued', now() - interval '12 min', 'default', NULL, NULL),
 ('u_tie_b', 'plain', 'enqueued', now() - interval '12 min', 'default', 'k', NULL),
 ('u_edge',  'plain', 'enqueued', now() - interval '1 microsecond', 'default', NULL, NULL),
 ('u_future','plain', 'enqueued', now() + interval '1 hour', 'default', NULL, NULL),
 ('u_other', 'plain', 'enqueued', now() - interval '50 min', 'other', NULL, NULL),
 ('d_1', 'parked', 'enqueued', now() - interval '40 min', 'default', NULL, NULL),
 ('d_2', 'parked', 'enqueued', now() - interval '40 min', 'default', 'p', 2),
 -- other statuses are never claimed and never counted
 ('done', 'grp', 'completed', now() - interval '2 hours', 'default', NULL, 1),
 ('dead', 'grp', 'failed', now() - interval '2 hours', 'default', NULL, 1);
ANALYZE autumn_jobs;
