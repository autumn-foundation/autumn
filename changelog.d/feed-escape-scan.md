### Performance

- Atom/RSS feed rendering scans text fields for characters needing XML escaping eight bytes per branch, cutting `Feed::render` instructions ~17% on a realistic 30-entry feed.
