FROM python:3.14-slim-bookworm AS builder
WORKDIR /app
COPY requirements.txt .
# Every dependency ships a CPython 3.14 manylinux wheel (psycopg-binary bundles
# libpq), so no compiler or libpq headers are needed; --only-binary makes a
# missing wheel fail the build instead of compiling from source.
RUN pip install --no-cache-dir --only-binary=:all: --prefix=/install -r requirements.txt

FROM python:3.14-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /install /usr/local
COPY . .

ENV DJANGO_SETTINGS_MODULE=benchapp.settings
ENV PYTHONDONTWRITEBYTECODE=1
ENV PORT=8080
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD curl -sf http://localhost:${PORT}/health || exit 1

CMD ["sh", "-c", \
  "python manage.py migrate --noinput && \
   gunicorn benchapp.asgi:application \
     --worker-class uvicorn_worker.UvicornWorker \
     --workers 4 \
     --bind 0.0.0.0:${PORT} \
     --log-level warning"]
