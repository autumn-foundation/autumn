{{/*
The chart name, or nameOverride.
*/}}
{{- define "app.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 55 | trimSuffix "-" -}}
{{- end -}}

{{/*
The object name: fullnameOverride, or the release name plus the chart name
when it is not part of it. At most 55 characters, so Flagger can add
-primary or -canary and stay within 63.
*/}}
{{- define "app.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 55 | trimSuffix "-" -}}
{{- else -}}
{{- $name := include "app.name" . -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 55 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 55 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "app.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "app.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "app.selectorLabels" -}}
app.kubernetes.io/name: {{ include "app.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Fail when no role is on: such a chart renders nothing. Fail on values that
moved under roles.<name>.
*/}}
{{- define "app.checkRoles" -}}
{{- if hasKey .Values "replicaCount" -}}
{{- fail "replicaCount moved to roles.<name>.replicaCount" -}}
{{- end -}}
{{- $on := 0 -}}
{{- range $name, $role := .Values.roles -}}
{{- if $role.enabled -}}
{{- $on = add1 $on -}}
{{- end -}}
{{- end -}}
{{- if eq $on 0 -}}
{{- fail "set roles.<name>.enabled=true for at least one role" -}}
{{- end -}}
{{- end -}}

{{/*
The object name of one role: the full name, cut to fit, then the role. At
most 55 characters. The argument is a dict with "root", "name" and "role".
*/}}
{{- define "app.roleName" -}}
{{- $role := .name | replace "_" "-" -}}
{{- $room := sub 54 (len $role) | int -}}
{{- if lt $room 1 -}}
{{- fail (printf "role %q is too long for a Kubernetes object name" .name) -}}
{{- end -}}
{{- printf "%s-%s" (include "app.fullname" .root | trunc $room | trimSuffix "-") $role -}}
{{- end -}}

{{/*
The selector of one role. Flagger selects on the "app" label first, so each
role has its own "app" value. Then the Flagger Services of a role do not
select the pods of another role.
*/}}
{{- define "app.roleSelectorLabels" -}}
app: {{ include "app.roleName" . }}
{{ include "app.selectorLabels" .root }}
app.kubernetes.io/component: {{ .name }}
{{- end -}}

{{- define "app.roleLabels" -}}
{{ include "app.labels" .root }}
app: {{ include "app.roleName" . }}
app.kubernetes.io/component: {{ .name }}
{{- end -}}

{{/*
Whether a role runs as an Argo Rollout. Only a role that serves HTTP does.
*/}}
{{- define "app.roleIsRollout" -}}
{{- if and .root.Values.rollout.enabled .role.servesHttp -}}true{{- end -}}
{{- end -}}

{{/*
The drain window and the grace period. Kubernetes kills the pod at the end of
terminationGracePeriodSeconds, so it must be longer than the drain window.
*/}}
{{- define "app.gracePeriod" -}}
{{- $s := .Values.shutdown -}}
{{- if lt (int $s.bufferSeconds) 1 -}}
{{- fail "shutdown.bufferSeconds must be 1 or more, so the grace period is longer than the drain window" -}}
{{- end -}}
{{- add $s.preStopSleepSeconds $s.prestopGraceSeconds $s.shutdownTimeoutSeconds $s.bufferSeconds -}}
{{- end -}}

{{/*
The trusted hosts of one role, comma-separated: trustedHosts, then the hosts
of the role Ingress. The prod profile does not start without them.
*/}}
{{- define "app.trustedHosts" -}}
{{- $hosts := .root.Values.trustedHosts | default list -}}
{{- if dig "ingress" "enabled" false .role -}}
{{- range .role.ingress.hosts -}}
{{- $hosts = append $hosts .host -}}
{{- end -}}
{{- end -}}
{{- if not $hosts -}}
{{- fail "set trustedHosts to the app's public host names; the prod profile does not start without [security.trusted_hosts]" -}}
{{- end -}}
{{- $hosts | uniq | join "," -}}
{{- end -}}

{{/*
The pod template of one role. The Deployment and the Rollout share it. The
argument is a dict with "root", "name" and "role".
*/}}
{{- define "app.podTemplate" -}}
{{- $root := .root -}}
{{- $role := .role -}}
{{- $values := $root.Values -}}
metadata:
  labels:
    {{- include "app.roleSelectorLabels" . | nindent 4 }}
  {{- $annotations := dict }}
  {{- if and $values.metrics.annotations (not $values.metrics.podMonitor.enabled) }}
  {{- $_ := set $annotations "prometheus.io/scrape" "true" }}
  {{- $_ := set $annotations "prometheus.io/path" $values.metrics.path }}
  {{- $_ := set $annotations "prometheus.io/port" (toString $values.containerPort) }}
  {{- end }}
  {{- /* Role annotations win over podAnnotations. podAnnotations win over the metrics annotations. */}}
  {{- $annotations = mustMergeOverwrite $annotations ($values.podAnnotations | default dict) ($role.podAnnotations | default dict) }}
  {{- with $annotations }}
  annotations:
    {{- /* Annotation values must be strings, also when --set gives a number. */}}
    {{- range $key, $value := . }}
    {{ $key }}: {{ $value | toString | quote }}
    {{- end }}
  {{- end }}
spec:
  {{- with $values.imagePullSecrets }}
  imagePullSecrets:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  terminationGracePeriodSeconds: {{ include "app.gracePeriod" $root }}
  # The app does not call the Kubernetes API.
  automountServiceAccountToken: false
  securityContext:
    runAsNonRoot: true
    runAsUser: 10001
    seccompProfile:
      type: RuntimeDefault
  containers:
    - name: app
      image: "{{ $values.image.repository }}:{{ $values.image.tag | default $root.Chart.AppVersion }}"
      imagePullPolicy: {{ $values.image.pullPolicy }}
      ports:
        # A worker role serves only the probes and the actuator on this port.
        - name: http
          containerPort: {{ $values.containerPort }}
          protocol: TCP
      env:
        - name: AUTUMN_ROLE
          value: {{ .name | quote }}
        {{- with $role.pin }}
        - name: AUTUMN_JOBS__PIN
          value: {{ join "," . | quote }}
        {{- end }}
        - name: AUTUMN_SERVER__HOST
          value: "0.0.0.0"
        # The pod IP is trusted too: Prometheus sends it as the Host header.
        - name: POD_IP
          valueFrom:
            fieldRef:
              fieldPath: status.podIP
        - name: AUTUMN_SECURITY__TRUSTED_HOSTS__HOSTS
          value: {{ printf "%s,$(POD_IP)" (include "app.trustedHosts" .) | quote }}
        - name: AUTUMN_SERVER__PORT
          value: {{ $values.containerPort | quote }}
        - name: AUTUMN_SERVER__PRESTOP_GRACE_SECS
          value: {{ $values.shutdown.prestopGraceSeconds | quote }}
        - name: AUTUMN_SERVER__SHUTDOWN_TIMEOUT_SECS
          value: {{ $values.shutdown.shutdownTimeoutSeconds | quote }}
        {{- if include "app.roleIsRollout" . }}
        # The pod-template hash becomes the `version` metric label. The
        # AnalysisTemplate from `autumn slo generate` selects the canary by it.
        - name: AUTUMN_DEPLOY_VERSION
          valueFrom:
            fieldRef:
              fieldPath: metadata.labels['rollouts-pod-template-hash']
        {{- end }}
        {{- with $values.env }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
        {{- with $role.env }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
      {{- with $values.envFrom }}
      envFrom:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      startupProbe:
        httpGet:
          path: {{ $values.probes.startupPath }}
          port: http
        periodSeconds: 2
        failureThreshold: 60
      livenessProbe:
        httpGet:
          path: {{ $values.probes.livePath }}
          port: http
        periodSeconds: 10
        failureThreshold: 3
      readinessProbe:
        httpGet:
          path: {{ $values.probes.readyPath }}
          port: http
        periodSeconds: 2
        failureThreshold: 1
      lifecycle:
        preStop:
          exec:
            command: ["sleep", "{{ $values.shutdown.preStopSleepSeconds }}"]
      securityContext:
        allowPrivilegeEscalation: false
        capabilities:
          drop: ["ALL"]
      {{- with ($role.resources | default $values.resources) }}
      resources:
        {{- toYaml . | nindent 8 }}
      {{- end }}
  {{- with ($role.nodeSelector | default $values.nodeSelector) }}
  nodeSelector:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with ($role.affinity | default $values.affinity) }}
  affinity:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with ($role.tolerations | default $values.tolerations) }}
  tolerations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end -}}
