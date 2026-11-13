import os
import dj_database_url

SECRET_KEY = os.environ.get("SECRET_KEY", "dev-insecure-key-change-in-prod")
DEBUG = False
ALLOWED_HOSTS = ["*"]

INSTALLED_APPS = [
    "django.contrib.contenttypes",
    "django.contrib.auth",
    "benchmark",
]

MIDDLEWARE = [
    "django.middleware.security.SecurityMiddleware",
    "whitenoise.middleware.WhiteNoiseMiddleware",
    "django.middleware.common.CommonMiddleware",
]

ROOT_URLCONF = "benchapp.urls"

TEMPLATES = [
    {
        "BACKEND": "django.template.backends.django.DjangoTemplates",
        "DIRS": [],
        "APP_DIRS": True,
        "OPTIONS": {"context_processors": []},
    }
]

# Under ASGI every request runs on a fresh thread, so persistent connections
# (CONN_MAX_AGE > 0) are never reused and pile up until Postgres refuses new
# ones. Use psycopg's connection pool instead, which Django supports natively
# and which requires CONN_MAX_AGE = 0. Five per worker process times the four
# Gunicorn workers gives the same 20 connections every other app gets.
DATABASES = {
    "default": dj_database_url.config(
        default=os.environ.get("DATABASE_URL", "postgres://benchmark:benchmark@localhost:5432/benchmark"),
        conn_max_age=0,
    )
}
DATABASES["default"]["OPTIONS"] = {"pool": {"min_size": 5, "max_size": 5, "timeout": 10}}

DEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"
USE_TZ = True
WSGI_APPLICATION = "benchapp.wsgi.application"
ASGI_APPLICATION = "benchapp.asgi.application"
LOGGING = {"version": 1, "disable_existing_loggers": True}
