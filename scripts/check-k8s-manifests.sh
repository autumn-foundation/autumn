#!/usr/bin/env bash
# Lint the Kubernetes release output and the golden SLO files (issue #3069).
#
# Usage:
#   scripts/check-k8s-manifests.sh              # run every check
#   scripts/check-k8s-manifests.sh --self-test  # prove that a bad manifest fails
#
# The checks run on the golden copies of `autumn release init --target
# kubernetes` in autumn-cli/tests/golden/release/. A Rust test keeps the
# copies equal to the generator output. There are two: an app with the web,
# worker and marketing roles, and an app with only the combined role.
#   1. `helm lint --strict` on each chart: default values, then each opt-in
#      mode (Argo Rollouts, Flagger, PodMonitor, Ingress, autoscaling).
#   2. `helm template | kubeconform -strict` for each of those modes.
#   3. One workload per role. Only a role that serves HTTP gets a Service.
#   4. The chart refuses bad values (Argo and Flagger together, no buffer, no
#      role on, an Ingress with no host).
#   5. `kustomize build | kubeconform -strict` on each Kustomize base.
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

roles_app="autumn-cli/tests/golden/release/kubernetes-roles/deploy"
combined_app="autumn-cli/tests/golden/release/kubernetes-combined/deploy"
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
    # A release host can answer 5xx for a minute. Retry every error with a
    # delay, for up to two minutes, and name a failed download as such.
    curl -fsSL --retry 8 --retry-delay 10 --retry-all-errors --retry-max-time 120 \
      -o "${dest}.part" "${url}" || fail "could not download ${url}"
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
  if [[ "$(uname -sm)" != "Linux x86_64" ]] \
    && [[ -z "${HELM:-}" || -z "${KUBECONFORM:-}" || -z "${KUSTOMIZE:-}" || -z "${PROMTOOL:-}" ]]; then
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
  expect_failure "a chart with no shutdown buffer" "bufferSeconds must be 1 or more" \
    "${HELM}" template demo "${roles_app}/helm" --set 'trustedHosts[0]=a.example.com' \
    --set shutdown.bufferSeconds=0
  echo "self-test passed"
}

main() {
  install_tools
  "${HELM}" version --short
  "${KUBECONFORM}" -v
  "${KUSTOMIZE}" version
  "${PROMTOOL}" --version | head -n 1

  # The chart requires trustedHosts; every mode sets it.
  local base_args="--set trustedHosts[0]=demo.example.com"
  local -a shared_modes=(
    "default::"
    "argo-rollouts::--set rollout.enabled=true --set analysis.templateName=demo-app-slo"
    "flagger::--set flagger.enabled=true -f ${golden}/helm-values.yaml"
    "pod-monitor::--set metrics.podMonitor.enabled=true"
    "flagger-gatewayapi::--set flagger.enabled=true --set flagger.provider=gatewayapi:v1 --set flagger.service.gatewayRefs[0].name=public -f ${golden}/helm-values.yaml"
    "pod-annotations::--set podAnnotations.prometheus\\.io/port=9000 --set podAnnotations.team=web"
  )
  local role_ingress="--set roles.web.ingress.enabled=true --set roles.web.ingress.hosts[0].host=shop.example.com --set roles.marketing.ingress.enabled=true --set roles.marketing.ingress.hosts[0].host=www.example.com --set roles.marketing.ingress.hosts[0].paths[0].path=/blog"
  local role_hpa="--set roles.web.autoscaling.enabled=true --set roles.worker.autoscaling.enabled=true"
  local -a roles_modes=(
    "${shared_modes[@]}"
    "ingress::${role_ingress}"
    "autoscaling::${role_hpa}"
    "argo-rollouts-autoscaling::--set rollout.enabled=true ${role_hpa}"
    "flagger-ingress-autoscaling::--set flagger.enabled=true --set flagger.provider=nginx ${role_ingress} ${role_hpa} -f ${golden}/helm-values.yaml"
    "pin::--set roles.worker.pin[0]=critical --set roles.worker.pin[1]=default"
    "flagger-pod-monitor::--set flagger.enabled=true --set metrics.podMonitor.enabled=true --set flagger.provider=nginx ${role_ingress} -f ${golden}/helm-values.yaml"
  )
  local -a combined_modes=(
    "${shared_modes[@]}"
    "flagger-pod-monitor::--set flagger.enabled=true --set metrics.podMonitor.enabled=true --set flagger.provider=nginx --set flagger.ingressRef.apiVersion=networking.k8s.io/v1 --set flagger.ingressRef.kind=Ingress --set flagger.ingressRef.name=demo -f ${golden}/helm-values.yaml"
    "ingress::--set roles.combined.ingress.enabled=true --set roles.combined.ingress.hosts[0].host=shop.example.com"
  )

  local mode name args app
  for app in roles combined; do
    local chart_dir modes_name
    if [[ "${app}" == roles ]]; then
      chart_dir="${roles_app}/helm"
      modes_name="roles_modes[@]"
    else
      chart_dir="${combined_app}/helm"
      modes_name="combined_modes[@]"
    fi
    for mode in "${!modes_name}"; do
      name="${app}-${mode%%::*}"
      args="${base_args} ${mode#*::}"
      echo "==> helm lint --strict (${name})"
      # shellcheck disable=SC2086 # args is a word list on purpose.
      "${HELM}" lint --strict "${chart_dir}" ${args}
      echo "==> helm template | kubeconform (${name})"
      # shellcheck disable=SC2086
      "${HELM}" template demo "${chart_dir}" ${args} > "${work}/${name}.yaml"
      validate "${work}/${name}.yaml"
    done
  done

  # count FILE PATTERN: the number of lines that are exactly PATTERN.
  count() {
    grep -cxF -- "$2" "$1" || true
  }
  # expect_count FILE PATTERN N MESSAGE
  expect_count() {
    local got
    got="$(count "$1" "$2")"
    [[ "${got}" == "$3" ]] || fail "$4 (${got} lines of \"$2\", not $3)"
  }

  local roles_default="${work}/roles-default.yaml"
  # One workload per role. Only web and marketing serve HTTP.
  expect_count "${roles_default}" "kind: Deployment" 3 "the roles chart needs one Deployment per role"
  expect_count "${roles_default}" "kind: Service" 2 "only the roles that serve HTTP get a Service"
  expect_count "${roles_default}" "kind: PodDisruptionBudget" 3 "each role needs a PodDisruptionBudget"
  for role in web worker marketing; do
    expect_count "${roles_default}" "              value: \"${role}\"" 1 "AUTUMN_ROLE=${role} is not set once"
    grep -qxF "      app.kubernetes.io/component: ${role}" "${roles_default}" \
      || fail "the ${role} selector has no component label"
    # Flagger selects on "app", so each role needs its own value.
    expect_count "${work}/roles-flagger.yaml" "      app: demo-demo-app-${role}" 1 \
      "the ${role} Deployment needs its own app label in its selector"
    expect_count "${work}/roles-flagger.yaml" "        app: demo-demo-app-${role}" 1 \
      "the ${role} pods need their own app label"
  done
  if grep -qxF "  name: demo-demo-app-worker" <(awk '/^kind: Service$/{s=1} /^---/{s=0} s' "${roles_default}"); then
    fail "the worker role got a Service"
  fi
  expect_count "${roles_default}" "              path: /live" 3 "each role needs a liveness probe"
  expect_count "${roles_default}" "              path: /ready" 3 "each role needs a readiness probe"
  expect_count "${work}/roles-ingress.yaml" "kind: Ingress" 2 "each HTTP role with ingress.enabled needs an Ingress"
  grep -qF 'value: "demo.example.com,www.example.com,$(POD_IP)"' "${work}/roles-ingress.yaml" \
    || fail "the Ingress hosts are not trusted hosts"
  expect_count "${work}/roles-autoscaling.yaml" "kind: HorizontalPodAutoscaler" 2 "each role with autoscaling needs an HPA"
  expect_count "${work}/roles-argo-rollouts.yaml" "kind: Rollout" 2 "each HTTP role needs a Rollout"
  expect_count "${work}/roles-argo-rollouts.yaml" "kind: Deployment" 1 "a worker stays a Deployment under Argo Rollouts"
  expect_count "${work}/roles-flagger.yaml" "kind: Canary" 2 "each HTTP role needs a Canary"
  expect_count "${work}/roles-flagger.yaml" "kind: Service" 0 "Flagger makes the Services"
  grep -qF 'value: "critical,default"' "${work}/roles-pin.yaml" \
    || fail "roles.worker.pin does not set AUTUMN_JOBS__PIN"
  # The web HPA scales the web Rollout. The worker HPA scales a Deployment.
  grep -A3 '^  scaleTargetRef:$' "${work}/roles-argo-rollouts-autoscaling.yaml" \
    | grep -A1 -F 'kind: Rollout' | grep -qxF '    name: demo-demo-app-web' \
    || fail "the web HPA does not scale the web Rollout"
  grep -A3 '^  scaleTargetRef:$' "${work}/roles-argo-rollouts-autoscaling.yaml" \
    | grep -A1 -F 'kind: Deployment' | grep -qxF '    name: demo-demo-app-worker' \
    || fail "the worker HPA does not scale the worker Deployment"
  expect_count "${work}/combined-default.yaml" "kind: Deployment" 1 "the combined chart needs one Deployment"
  expect_count "${work}/combined-default.yaml" "kind: Service" 1 "the combined role serves HTTP"

  local default="${work}/combined-default.yaml"
  # The grace period is the sum of the shutdown values: 5 + 5 + 30 + 10.
  grep -q 'terminationGracePeriodSeconds: 50$' "${default}" \
    || fail "the default grace period is not 50 s"
  grep -q 'kind: PodDisruptionBudget' "${default}" \
    || fail "the chart has no PodDisruptionBudget"
  grep -q 'podTemplateHashValue: Latest' "${work}/combined-argo-rollouts.yaml" \
    || fail "the Rollout does not pass the canary hash to the analysis"
  grep -q 'name: shop-availability' "${work}/combined-flagger.yaml" \
    || fail "the Flagger Canary does not use the generated MetricTemplates"

  grep -q 'kind: PodMonitor' "${work}/combined-flagger-pod-monitor.yaml" \
    || fail "Flagger mode has no PodMonitor"
  grep -q 'prometheus.io/scrape' "${default}" \
    || fail "the default pod has no scrape annotations"
  grep -q 'jobLabel: app.kubernetes.io/instance' "${work}/combined-pod-monitor.yaml" \
    || fail "the PodMonitor does not set job to the release name"
  grep -q 'prometheus.io/port: "9000"' "${work}/combined-pod-annotations.yaml" \
    || fail "podAnnotations do not override the scrape annotations"
  if grep -q 'prometheus.io/scrape' "${work}/combined-pod-monitor.yaml"; then
    fail "a pod with a PodMonitor also has scrape annotations"
  fi
  grep -q 'automountServiceAccountToken: false' "${default}" \
    || fail "the pod mounts a service account token"
  grep -q 'value: "demo.example.com,$(POD_IP)"' "${default}" \
    || fail "the trusted hosts do not include the configured host and the pod IP"

  local chart="${roles_app}/helm"
  expect_failure "a chart with no trusted hosts" "set trustedHosts" \
    "${HELM}" template demo "${chart}"
  # shellcheck disable=SC2086 # base_args is a word list on purpose.
  expect_failure "Argo Rollouts and Flagger together" "not both" \
    "${HELM}" template demo "${chart}" ${base_args} --set rollout.enabled=true --set flagger.enabled=true
  # shellcheck disable=SC2086
  expect_failure "a shutdown buffer of 0" "bufferSeconds must be 1 or more" \
    "${HELM}" template demo "${chart}" ${base_args} --set shutdown.bufferSeconds=0
  # shellcheck disable=SC2086
  expect_failure "a chart with no role on" "at least one role" \
    "${HELM}" template demo "${chart}" ${base_args} --set roles.web.enabled=false \
    --set roles.worker.enabled=false --set roles.marketing.enabled=false
  # shellcheck disable=SC2086
  expect_failure "a top-level replicaCount" "replicaCount moved" \
    "${HELM}" template demo "${chart}" ${base_args} --set replicaCount=3
  # shellcheck disable=SC2086
  expect_failure "one flagger.ingressRef for two HTTP roles" "can serve one role only" \
    "${HELM}" template demo "${chart}" ${base_args} --set flagger.enabled=true \
    --set flagger.ingressRef.name=shop
  # shellcheck disable=SC2086
  expect_failure "an Ingress with no host" "roles.web.ingress.hosts" \
    "${HELM}" template demo "${chart}" ${base_args} --set roles.web.ingress.enabled=true

  for app in "${roles_app}" "${combined_app}"; do
    echo "==> kustomize build | kubeconform (${app})"
    "${KUSTOMIZE}" build "${app}/kustomize/base" > "${work}/kustomize.yaml"
    validate "${work}/kustomize.yaml"
  done
  "${KUSTOMIZE}" build "${roles_app}/kustomize/base" > "${work}/kustomize-roles.yaml"
  expect_count "${work}/kustomize-roles.yaml" "kind: Deployment" 3 "the Kustomize base needs one Deployment per role"
  expect_count "${work}/kustomize-roles.yaml" "kind: Service" 2 "only the Kustomize roles that serve HTTP get a Service"
  expect_count "${work}/kustomize-roles.yaml" "kind: Ingress" 2 "only the Kustomize roles that serve HTTP get an Ingress"
  for role in web worker marketing; do
    expect_count "${work}/kustomize-roles.yaml" "          value: ${role}" 1 "Kustomize AUTUMN_ROLE=${role} is not set once"
  done

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
