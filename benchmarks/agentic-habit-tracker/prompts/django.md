Prompt-Version: v1

# Build brief: Habit Tracker — Django

Read `prompts/_core.md` and `SPEC.md` first; they define the app, the exact JSON
API contract, and the streak semantics. Everything there applies. Below are the
Django-specific bootstrapping notes.

## Framework facts

- Use **Django** (with **Django REST Framework** for the JSON API is fine, or
  plain `JsonResponse` views — your choice, as long as the `SPEC.md` contract is
  met exactly).
- Scaffold: `django-admin startproject habittracker` then
  `python manage.py startapp habits`.
- **Models.** A `Habit` model (`name`, `description` nullable, `created_at`
  `auto_now_add`) and a `Completion` model (`habit` FK with
  `on_delete=CASCADE`, `date` a `DateField`). Add
  `unique_together = ("habit", "date")` (or a `UniqueConstraint`) so a duplicate
  `(habit, date)` completion raises `IntegrityError` → map to **409**.
- **Migrations.** `python manage.py makemigrations && migrate`. SQLite is fine
  for the benchmark; Postgres is also acceptable.
- **URLs / views.**
  - `POST /api/habits`, `GET /api/habits`, `GET /api/habits/<id>`,
    `PUT /api/habits/<id>`, `DELETE /api/habits/<id>`,
    `POST /api/habits/<id>/complete` — all JSON.
  - Return the documented status codes explicitly (`status=201`, `204`, `409`,
    `422`/`400`, `404`). DRF's `Response(status=...)` or `JsonResponse(status=...)`.
  - `GET /` — a Django template view (`TemplateView` or a function view calling
    `render(...)`) returning `text/html` listing habits.
- **Validation.** Reject empty `name` (serializer/`clean`) and malformed dates
  (`datetime.date.fromisoformat` raises `ValueError` → 422/400).
- **Streak.** Compute `current_streak` per `SPEC.md` §3 in Python from the
  completion dates. `history` = the dates sorted descending as ISO strings.
- **Seed / demo data.** A data migration or a `manage.py` custom command (e.g.
  `python manage.py seed`) creating a couple of demo habits with completions.
- **Tests.** `python manage.py test` with Django's `TestCase` / DRF `APIClient`.

## run.sh

```sh
#!/usr/bin/env sh
set -e
python -m pip install -r requirements.txt
python manage.py migrate
python manage.py seed        # your seed command
exec python manage.py runserver 0.0.0.0:"${PORT:-8080}"
```

`runserver` blocks while the server runs — good. Make sure `ALLOWED_HOSTS`
permits `localhost`/`127.0.0.1`.
