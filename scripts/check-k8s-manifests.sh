#!/usr/bin/env bash
# Lint the Kubernetes release templates and the golden SLO files (issue #3069).
#
# Usage:
#   scripts/check-k8s-manifests.sh              # run every check
#   scripts/check-k8s-manifests.sh --self-test  # prove that a bad manifest fails
#
# The checks:
#   1. Render the `autumn release init --target kubernetes` templates.
#   2. `helm lint --strict` on the chart: default values, then each opt-in mode
#      (Argo Rollouts, Flagger, PodMonitor, Flagger with a PodMonitor).
#   3. `helm template | kubeconform -strict` for each of those modes.
#   4. The chart refuses bad values (Argo and Flagger together, no buffer).
#   5. `kustomize build | kubeconform -strict` on the Kustomize base.
#   6. `kubeconform -strict` on the golden PrometheusRule, AnalysisTemplate and
#      MetricTemplates from `autumn slo generate`.
#   7. `promtool check rules` and `promtool test rules` on the golden rules.
#
# Tools: the script downloads pinned linux-amd64 versions. It keeps the
# tarballs in K8S_TOOLS_DIR (default: target/k8s-tools) and checks their
# SHA-256 on every run, also when they come from a cache. On another platform,
# set HELM, KUBECONFORM, KUSTOMIZE and PROMTOOL to your own binaries.
#
# Schemas come from two pinned commits: Kubernetes from
# yannh/kubernetes-json-schema, and the CRDs (Argo Rollouts, Flagger,
# prometheus-operator) from datreeio/CRDs-catalog. kubeconform caches them in
# K8S_TOOLS_DIR/schemas.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

HELM_VERSION="3.16.4"
HELM_SHA256="fc307327959aa38ed8f9f7e66d45492bb022a66c3e5da6063958254b9767d179"
KUBECONFORM_VERSION="0.6.7"
KUBECONFORM_SHA256="95f14e87aa28c09d5941f11bd024c1d02fdc0303ccaa23f61cef67bc92619d73"
KUSTOMIZE_VERSION="5.4.3"
KUSTOMIZE_SHA256="3669470b454d865c8184d6bce78df05e977c9aea31c30df3c669317d43bcc7a7"
PROMETHEUS_VERSION="3.5.0"
PROMETHEUS_SHA256="e811827af26d822afb09a4f28314f61b618b12cff5369835a67f674d8b46f39a"
KUBERNETES_VERSION="1.31.0"
K8S_SCHEMA_COMMIT="8df8a883b68a24a104b4a9e43c1288090ae60b3b"
CRD_CATALOG_COMMIT="fd90051867733c60d32d16450556e9cd18459aef"
K8S_SCHEMAS="https://raw.githubusercontent.com/yannh/kubernetes-json-schema/${K8S_SCHEMA_COMMIT}/{{.NormalizedKubernetesVersion}}-standalone{{.StrictSuffix}}/{{.ResourceKind}}{{.KindSuffix}}.json"
CRD_CATALOG="https://raw.githubusercontent.com/datreeio/CRDs-catalog/${CRD_CATALOG_COMMIT}/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json"

templates="autumn-cli/src/templates/release/kubernetes"
golden="autumn-cli/tests/golden/slo"
promtool_test="autumn-cli/tests/fixtures/slo/burn-rate.test.yaml"

tools_dir="${K8S_TOOLS_DIR:-${root}/target/k8s-tools}"
work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

fail() {
  echo "error: $*" >&2
  exit 1
}

sha256() {
  if command -v sha256sum > /dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch URL SHA256 NAME: keep the tarball in tools_dir, and check its SHA-256
# on every run. Print its path.
fetch() {
  local url="$1" sha="$2" dest="${tools_dir}/$3"
  if [[ ! -f "${dest}" ]]; then
    curl -fsSL --retry 3 -o "${dest}.part" "${url}"
    mv "${dest}.part" "${dest}"
  fi
  if [[ "$(sha256 "${dest}")" != "${sha}" ]]; then
    rm -f "${dest}"
    fail "checksum mismatch for ${url}"
  fi
  echo "${dest}"
}

install_tools() {
  mkdir -p "${tools_dir}/schemas" "${work}/bin"
  if [[ -z "${HELM:-}${KUBECONFORM:-}${KUSTOMIZE:-}${PROMTOOL:-}" ]] \
    && [[ "$(uname -sm)" != "Linux x86_64" ]]; then
    fail "the pinned tools are linux-amd64 only; set HELM, KUBECONFORM, KUSTOMIZE and PROMTOOL"
  fi
  local tgz
  if [[ -z "${HELM:-}" ]]; then
    tgz="$(fetch "https://get.helm.sh/helm-v${HELM_VERSION}-linux-amd64.tar.gz" \
      "${HELM_SHA256}" "helm-${HELM_VERSION}.tgz")"
    tar -xzf "${tgz}" -C "${work}" linux-amd64/helm
    HELM="${work}/linux-amd64/helm"
  fi
  if [[ -z "${KUBECONFORM:-}" ]]; then
    tgz="$(fetch "https://github.com/yannh/kubeconform/releases/download/v${KUBECONFORM_VERSION}/kubeconform-linux-amd64.tar.gz" \
      "${KUBECONFORM_SHA256}" "kubeconform-${KUBECONFORM_VERSION}.tgz")"
    tar -xzf "${tgz}" -C "${work}/bin" kubeconform
    KUBECONFORM="${work}/bin/kubeconform"
  fi
  if [[ -z "${KUSTOMIZE:-}" ]]; then
    tgz="$(fetch "https://github.com/kubernetes-sigs/kustomize/releases/download/kustomize%2Fv${KUSTOMIZE_VERSION}/kustomize_v${KUSTOMIZE_VERSION}_linux_amd64.tar.gz" \
      "${KUSTOMIZE_SHA256}" "kustomize-${KUSTOMIZE_VERSION}.tgz")"
    tar -xzf "${tgz}" -C "${work}/bin" kustomize
    KUSTOMIZE="${work}/bin/kustomize"
  fi
  if [[ -z "${PROMTOOL:-}" ]]; then
    local dir="prometheus-${PROMETHEUS_VERSION}.linux-amd64"
    tgz="$(fetch "https://github.com/prometheus/prometheus/releases/download/v${PROMETHEUS_VERSION}/${dir}.tar.gz" \
      "${PROMETHEUS_SHA256}" "prometheus-${PROMETHEUS_VERSION}.tgz")"
    tar -xzf "${tgz}" -C "${work}" "${dir}/promtool"
    PROMTOOL="${work}/${dir}/promtool"
  fi
}

validate() {
  "${KUBECONFORM}" -strict -summary -kubernetes-version "${KUBERNETES_VERSION}" \
    -cache "${tools_dir}/schemas" \
    -schema-location "${K8S_SCHEMAS}" -schema-location "${CRD_CATALOG}" "$@"
}

# Render the release templates as `autumn release init` does for a project
# named demo_app. A Rust test (kubernetes_templates_use_only_placeholders_the_
# ci_check_knows) keeps this list of placeholders complete.
render() {
  local out="$1"
  mkdir -p "${out}"
  cp -R "${templates}/." "${out}/"
  while IFS= read -r -d '' file; do
    sed -e 's/{{k8s_name}}/demo-app/g' -e 's/{{project_name}}/demo_app/g' "${file}" > "${file%.tmpl}"
    rm "${file}"
  done < <(find "${out}" -name '*.tmpl' -print0)
  if grep -rnE '\{\{(k8s_name|project_name)\}\}' "${out}"; then
    fail "a release placeholder was not replaced"
  fi
}

# expect_failure DESCRIPTION REASON COMMAND...: the command must exit non-zero
# and print REASON, so an unrelated error cannot pass the check.
expect_failure() {
  local description="$1" reason="$2"
  shift 2
  if "$@" > "${work}/expected-failure.log" 2>&1; then
    fail "${description}: the command passed, but it must fail"
  fi
  if ! grep -qF -- "${reason}" "${work}/expected-failure.log"; then
    cat "${work}/expected-failure.log" >&2
    fail "${description}: the command failed, but not with \"${reason}\""
  fi
  echo "ok: ${description} is refused"
}

self_test() {
  install_tools
  cat > "${work}/bad.yaml" <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata:
  name: bad
spec:
  replicas: "two"
  selector: {}
  template: {}
  notAField: true
EOF
  expect_failure "kubeconform on an invalid Deployment" "is invalid" validate "${work}/bad.yaml"
  render "${work}/chart"
  expect_failure "a chart with no shutdown buffer" "bufferSeconds must be 1 or more" \
    "${HELM}" template demo "${work}/chart/helm" --set shutdown.bufferSeconds=0
  echo "self-test passed"
}

main() {
  install_tools
  "${HELM}" version --short
  "${KUBECONFORM}" -v
  "${KUSTOMIZE}" version
  "${PROMTOOL}" --version | head -n 1

  local rendered="${work}/rendered"
  render "${rendered}"
  local chart="${rendered}/helm"

  local -a modes=(
    "default::"
    "argo-rollouts::--set rollout.enabled=true --set analysis.templateName=demo-app-slo"
    "flagger::--set flagger.enabled=true -f ${golden}/helm-values.yaml"
    "pod-monitor::--set metrics.podMonitor.enabled=true"
    "flagger-pod-monitor::--set flagger.enabled=true --set metrics.podMonitor.enabled=true --set flagger.provider=nginx -f ${golden}/helm-values.yaml"
  )
  local mode name args
  for mode in "${modes[@]}"; do
    name="${mode%%::*}"
    args="${mode#*::}"
    echo "==> helm lint --strict (${name})"
    # shellcheck disable=SC2086 # args is a word list on purpose.
    "${HELM}" lint --strict "${chart}" ${args}
    echo "==> helm template | kubeconform (${name})"
    # shellcheck disable=SC2086
    "${HELM}" template demo "${chart}" ${args} > "${work}/${name}.yaml"
    validate "${work}/${name}.yaml"
  done

  # The grace period is the sum of the shutdown values: 5 + 5 + 30 + 10.
  grep -q 'terminationGracePeriodSeconds: 50$' "${work}/default.yaml" \
    || fail "the default grace period is not 50 s"
  grep -q 'kind: PodDisruptionBudget' "${work}/default.yaml" \
    || fail "the chart has no PodDisruptionBudget"
  grep -q 'podTemplateHashValue: Latest' "${work}/argo-rollouts.yaml" \
    || fail "the Rollout does not pass the canary hash to the analysis"
  grep -q 'name: shop-availability' "${work}/flagger.yaml" \
    || fail "the Flagger Canary does not use the generated MetricTemplates"

  grep -q 'kind: PodMonitor' "${work}/flagger-pod-monitor.yaml" \
    || fail "Flagger mode has no PodMonitor"
  if grep -q 'prometheus.io/scrape' "${work}/pod-monitor.yaml"; then
    fail "a pod with a PodMonitor also has scrape annotations"
  fi
  grep -q 'automountServiceAccountToken: false' "${work}/default.yaml" \
    || fail "the pod mounts a service account token"

  expect_failure "Argo Rollouts and Flagger together" "not both" \
    "${HELM}" template demo "${chart}" --set rollout.enabled=true --set flagger.enabled=true
  expect_failure "a shutdown buffer of 0" "bufferSeconds must be 1 or more" \
    "${HELM}" template demo "${chart}" --set shutdown.bufferSeconds=0

  echo "==> kustomize build | kubeconform"
  "${KUSTOMIZE}" build "${rendered}/kustomize" > "${work}/kustomize.yaml"
  validate "${work}/kustomize.yaml"

  echo "==> kubeconform on the golden SLO objects"
  validate "${golden}/prometheus-rule.yaml" "${golden}/argo-analysis-template.yaml" \
    "${golden}/flagger-metric-templates.yaml"

  echo "==> promtool"
  "${PROMTOOL}" check rules "${golden}/prometheus-rules.yaml"
  "${PROMTOOL}" test rules "${promtool_test}"

  echo "Kubernetes manifests and SLO files are valid."
}

case "${1:-}" in
  --self-test) self_test ;;
  "") main ;;
  *) fail "unknown argument: $1" ;;
esac
