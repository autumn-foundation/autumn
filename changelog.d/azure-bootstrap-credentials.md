### Security

- **release:** `autumn release init --target azure-container-apps` no longer
  gives production credentials to the Container App while it runs the public
  bootstrap image (#2314). The generated `main.tf` creates the app with no
  managed identity, no registry and no Key Vault secret refs. A new generated
  script, `azure-cutover.sh`, copies them from the migration job and sets the
  real image in one write. It opens external ingress only when the new
  revision runs the real image. `azure-deploy.yml` and the manual walkthrough
  both run it. Terraform now ignores the app's env vars after it creates the
  app. To change one, use `az containerapp update --set-env-vars`. If you
  regenerate the workflow for an existing app, also regenerate `main.tf` and
  run `terraform apply` first: the script needs the full secret set on the
  migration job.
