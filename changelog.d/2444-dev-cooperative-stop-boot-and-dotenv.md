### Fixed

- **cli:** on Windows, `autumn dev` no longer force-kills an app that is still
  starting (issue #2444). It waits for the app to finish booting, then starts
  the stop timer. It kills an app that is still starting after 15 minutes.
- **cli:** the `autumn dev` stop budget reads `.env` from the selected
  package, not from the CLI's working directory.
