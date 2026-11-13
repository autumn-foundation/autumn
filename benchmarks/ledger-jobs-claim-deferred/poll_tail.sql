UPDATE autumn_jobs SET status = 'succeeded', finished_at = NOW() WHERE id = :jid;
