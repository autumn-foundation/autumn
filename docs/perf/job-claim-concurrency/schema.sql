-- autumn_jobs as the shipped migrations leave it (autumn/migrations/*jobs*).
DROP TABLE IF EXISTS autumn_jobs;
\ir ../../../autumn/migrations/20260513000000_create_job_queue/up.sql
\ir ../../../autumn/migrations/20260519000000_add_trace_context_to_jobs/up.sql
\ir ../../../autumn/migrations/20260610000000_add_job_uniqueness_concurrency/up.sql
\ir ../../../autumn/migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql
\ir ../../../autumn/migrations/20260628000000_add_queue_to_jobs/up.sql
\ir ../../../autumn/migrations/20261026170000_add_jobs_queue_name_ready_index/up.sql
