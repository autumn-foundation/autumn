### Performance

- **commentable:** the paged thread read (`comment_thread_page`, and the
  `commentable::router()` `GET`) reads each comment once. Its recursive walk
  now carries every column, so the statement no longer joins each comment
  back on its primary key. A page reads about a third fewer shared buffers
  (−33% over a workload of seven thread sizes, from 15 to 150,000 comments).
  The rows are unchanged. See `docs/perf/comment-thread-page`.
