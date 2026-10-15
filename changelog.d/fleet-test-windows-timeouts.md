### Testing

- **db:** the fleet "stranded drain" tests wait up to 60 s, not 5 s. A database
  create takes seconds on the Windows CI runner, and the 5 s bound failed there.
