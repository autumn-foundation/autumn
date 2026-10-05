### Security

- **release:** `autumn release init --target azure-container-apps` no longer
  gives production credentials to the public bootstrap image (#2314). The
  generated `main.tf` creates the Container App with no managed identity, no
  registry and no Key Vault secret refs. The deploy step in `azure-deploy.yml`
  (and the manual walkthrough) copies them from the migration job when it
  sets the real image. Terraform now ignores the app's env vars after create.
