### Fixed

- `autumn release init --target azure-container-apps`: the generated
  `main.tf` now keeps external ingress **disabled** until the first real
  image is deployed. Previously `external_enabled = true` was set from the
  initial `terraform apply`, so between apply and cutover an inbound
  request to the public FQDN could start the bootstrap placeholder revision
  — with production secret references and the Key Vault-capable managed
  identity attached. `min_replicas = 0` only permits scale-to-zero; it does
  not stop the HTTP scale rule waking the placeholder on traffic
  ([#2312](https://github.com/autumn-foundation/autumn/issues/2312)).
  The generated `azure-deploy.yml` workflow (and the deployment guide's
  manual cutover) now enable external ingress with
  `az containerapp ingress enable` after `az containerapp update --image`
  lands the real image.
