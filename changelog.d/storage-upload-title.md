### Documentation

- **"File uploads" now lands on the storage guide, not the multi-replica
  caveat:** `docs/guide/storage.md` is the page that answers "how do I accept
  a file upload" — its opening sentence is "apps that accept user-uploaded
  files", it carries "Scaffolded uploads" and "Direct uploads", and it ships a
  working `Local` backend that needs no configuration. Its H1 was "File
  Storage in Autumn", so the reader's own word for the task appeared in
  neither its slug nor its title. The only place in all 164 guide pages
  announcing both words was `cloud-native.md`'s "## File Uploads", a
  multi-replica caveat whose advice is "enable the `storage` feature and pick
  the `S3` backend" — so a reader arriving from a search engine read a
  deployment constraint as the answer, and could reasonably conclude S3 is
  required to accept an upload at all. Four pages already hand-routed the
  question to `storage.md` (`fleet-deploys.md`'s "Uploaded files" row,
  `forms.md`'s "## Uploads", `cloud-native.md`'s own pointer, and the guide
  index, which describes the page as "uploading a file" — the reader's word
  the page itself never used). Retitled to "File Uploads and Storage". No page
  was added, moved or deleted, no URL or anchor changed, and the question is
  now pinned in `scripts/docs-retrieval-questions.tsv` so a future retitle
  cannot take it away again.
