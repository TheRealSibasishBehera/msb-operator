{{/* Chart name, overridable via nameOverride. */}}
{{- define "msb-operator.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Common labels stamped on every rendered object. */}}
{{- define "msb-operator.labels" -}}
app.kubernetes.io/name: {{ include "msb-operator.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{/* Controller selector labels. */}}
{{- define "msb-operator.controller.selectorLabels" -}}
app.kubernetes.io/name: msb-controller
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Daemon selector labels. */}}
{{- define "msb-operator.daemon.selectorLabels" -}}
app.kubernetes.io/name: msb-daemon
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Gateway selector labels. */}}
{{- define "msb-operator.gateway.selectorLabels" -}}
app.kubernetes.io/name: msb-gateway
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Build a fully-qualified image reference: <registry>/<repo>:<tag>.
Call with a dict {registry, repo, tag, defaultTag} — tag falls back to
defaultTag (the chart appVersion) when empty.
*/}}
{{- define "msb-operator.image" -}}
{{- $tag := .tag | default .defaultTag -}}
{{- printf "%s/%s:%s" .registry .repo $tag -}}
{{- end -}}

{{/* The controller ServiceAccount name. */}}
{{- define "msb-operator.controller.serviceAccountName" -}}
msb-controller
{{- end -}}

{{/* The gateway ServiceAccount name. */}}
{{- define "msb-operator.gateway.serviceAccountName" -}}
msb-gateway
{{- end -}}
