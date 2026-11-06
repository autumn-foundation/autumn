from django.db import migrations, models

# The shared benchmark database is initialised from schema/init.sql, which
# already creates `posts` and `api_tokens`. The tables are therefore created
# only if they are missing (a fresh standalone database), while Django's
# migration state still records the models below.
CREATE_TABLES = """
CREATE TABLE IF NOT EXISTS posts (
    id          BIGSERIAL    PRIMARY KEY,
    title       TEXT         NOT NULL,
    body        TEXT         NOT NULL,
    published   BOOLEAN      NOT NULL DEFAULT FALSE,
    author      TEXT         NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
CREATE TABLE IF NOT EXISTS api_tokens (
    id          BIGSERIAL    PRIMARY KEY,
    token       TEXT         NOT NULL UNIQUE,
    principal   TEXT         NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);
"""

DROP_TABLES = """
DROP TABLE IF EXISTS api_tokens;
DROP TABLE IF EXISTS posts;
"""


class Migration(migrations.Migration):
    initial = True

    dependencies = []

    operations = [
        migrations.SeparateDatabaseAndState(
            database_operations=[
                migrations.RunSQL(CREATE_TABLES, reverse_sql=DROP_TABLES),
            ],
            state_operations=[
                migrations.CreateModel(
                    name="Post",
                    fields=[
                        ("id", models.BigAutoField(auto_created=True, primary_key=True, serialize=False, verbose_name="ID")),
                        ("title", models.TextField()),
                        ("body", models.TextField()),
                        ("published", models.BooleanField(default=False)),
                        ("author", models.TextField()),
                        ("created_at", models.DateTimeField(auto_now_add=True)),
                        ("updated_at", models.DateTimeField(auto_now=True)),
                    ],
                    options={"db_table": "posts", "ordering": ["-created_at"]},
                ),
                migrations.CreateModel(
                    name="ApiToken",
                    fields=[
                        ("id", models.BigAutoField(auto_created=True, primary_key=True, serialize=False, verbose_name="ID")),
                        ("token", models.TextField(unique=True)),
                        ("principal", models.TextField()),
                        ("created_at", models.DateTimeField(auto_now_add=True)),
                    ],
                    options={"db_table": "api_tokens"},
                ),
            ],
        ),
    ]
