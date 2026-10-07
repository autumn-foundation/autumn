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
#      (Argo Rollouts, Flagger, ServiceMonitor).
#   3. `helm template | kubeconform -strict` for each of those modes.
#   4. The chart refuses bad values (Argo and Flagger together, no buffer).
#   5. `kustomize build | kubeconform -strict` on the Kustomize base.
#   6. `kubeconform -strict` on the golden PrometheusRule, AnalysisTemplate and
#      MetricTemplates from `autumn slo generate`.
#   7. `promtool check rules` and `promtool test rules` on the golden rules.
#
# Tools: the script downloads pinned versions and checks their SHA-256. Set
# HELM, KUBECONFORM, KUSTOMIZE or PROMTOOL to use your own binary. Set
# K8S_TOOLS_DIR to change the download directory.
#
# CRD schemas (Argo Rollouts, Flagger, prometheus-operator) come from the
# datreeio/CRDs-catalog. Kubernetes schemas come from kubeconform's default.

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
CRD_CATALOG='https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'

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

# download URL SHA256 DEST: fetch URL to DEST and check its SHA-256.
download() {
  local url="$1" sha="$2" dest="$3"
  curl -fsSL --retry 3 -o "${dest}" "${url}"
  echo "${sha}  ${dest}" | sha256sum -c --quiet - || fail "checksum mismatch for ${url}"
}

install_tools() {
  mkdir -p "${tools_dir}"
  if [[ -z "${HELM:-}" ]]; then
    HELM="${tools_dir}/helm-${HELM_VERSION}"
    if [[ ! -x "${HELM}" ]]; then
      download "https://get.helm.sh/helm-v${HELM_VERSION}-linux-amd64.tar.gz" \
        "${HELM_SHA256}" "${work}/helm.tgz"
      tar -xzf "${work}/helm.tgz" -C "${work}" linux-amd64/helm
      mv "${work}/linux-amd64/helm" "${HELM}"
    fi
  fi
  if [[ -z "${KUBECONFORM:-}" ]]; then
    KUBECONFORM="${tools_dir}/kubeconform-${KUBECONFORM_VERSION}"
    if [[ ! -x "${KUBECONFORM}" ]]; then
      download "https://github.com/yannh/kubeconform/releases/download/v${KUBECONFORM_VERSION}/kubeconform-linux-amd64.tar.gz" \
        "${KUBECONFORM_SHA256}" "${work}/kubeconform.tgz"
      tar -xzf "${work}/kubeconform.tgz" -C "${work}" kubeconform
      mv "${work}/kubeconform" "${KUBECONFORM}"
    fi
  fi
  if [[ -z "${KUSTOMIZE:-}" ]]; then
    KUSTOMIZE="${tools_dir}/kustomize-${KUSTOMIZE_VERSION}"
    if [[ ! -x "${KUSTOMIZE}" ]]; then
      download "https://github.com/kubernetes-sigs/kustomize/releases/download/kustomize%2Fv${KUSTOMIZE_VERSION}/kustomize_v${KUSTOMIZE_VERSION}_linux_amd64.tar.gz" \
        "${KUSTOMIZE_SHA256}" "${work}/kustomize.tgz"
      tar -xzf "${work}/kustomize.tgz" -C "${work}" kustomize
      mv "${work}/kustomize" "${KUSTOMIZE}"
    fi
  fi
  if [[ -z "${PROMTOOL:-}" ]]; then
    PROMTOOL="${tools_dir}/promtool-${PROMETHEUS_VERSION}"
    if [[ ! -x "${PROMTOOL}" ]]; then
      local dir="prometheus-${PROMETHEUS_VERSION}.linux-amd64"
      download "https://github.com/prometheus/prometheus/releases/download/v${PROMETHEUS_VERSION}/${dir}.tar.gz" \
        "${PROMETHEUS_SHA256}" "${work}/prometheus.tgz"
      tar -xzf "${work}/prometheus.tgz" -C "${work}" "${dir}/promtool"
      mv "${work}/${dir}/promtool" "${PROMTOOL}"
    fi
  fi
}

validate() {
  "${KUBECONFORM}" -strict -summary -kubernetes-version "${KUBERNETES_VERSION}" \
    -schema-location default -schema-location "${CRD_CATALOG}" "$@"
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

# expect_failure DESCRIPTION COMMAND...: the command must exit non-zero.
expect_failure() {
  local description="$1"
  shift
  if "$@" > "${work}/expected-failure.log" 2>&1; then
    fail "${description}: the command passed, but it must fail"
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
  expect_failure "kubeconform on an invalid Deployment" validate "${work}/bad.yaml"
  render "${work}/chart"
  expect_failure "a chart with no shutdown buffer" \
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
    "service-monitor::--set metrics.serviceMonitor.enabled=true"
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

  expect_failure "Argo Rollouts and Flagger together" \
    "${HELM}" template demo "${chart}" --set rollout.enabled=true --set flagger.enabled=true
  expect_failure "a shutdown buffer of 0" \
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
