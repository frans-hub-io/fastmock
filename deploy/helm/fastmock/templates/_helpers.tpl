{{- define "fastmock.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "fastmock.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "fastmock.selector" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: server
{{- end -}}

{{- define "fastmock.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{- define "fastmock.pluginEnvName" -}}
{{- printf "FASTMOCK_PLUGIN_%s_%s" (.plugin | upper | replace "-" "_") (.key | upper | replace "-" "_" | replace "." "_") -}}
{{- end -}}

{{- define "fastmock.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "fastmock.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- required "serviceAccount.name is required when serviceAccount.create is false" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "fastmock.dependencyFullname" -}}
{{- $root := .root -}}
{{- if contains .name $root.Release.Name -}}
{{- $root.Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" $root.Release.Name .name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end }}

{{- define "fastmock.databaseCaSecret" -}}
{{- if .Values.storage.database.tls.caSecret -}}
{{- .Values.storage.database.tls.caSecret -}}
{{- else if .Values.cnpg.enabled -}}
{{- printf "%s-ca" (include "fastmock.dependencyFullname" (dict "root" . "name" "cnpg")) -}}
{{- end -}}
{{- end }}

{{- define "fastmock.databaseCaKey" -}}
{{- if .Values.storage.database.tls.caSecret -}}
{{- .Values.storage.database.tls.caKey -}}
{{- else -}}
ca.crt
{{- end -}}
{{- end }}

{{- define "fastmock.databaseSslMode" -}}
{{- if .Values.storage.database.tls.mode -}}
{{- .Values.storage.database.tls.mode -}}
{{- else if .Values.cnpg.enabled -}}
verify-full
{{- end -}}
{{- end }}

{{- define "fastmock.kafkaBootstrap" -}}
{{- if .Values.kafka.enabled -}}
{{- printf "%s:9092" (include "fastmock.dependencyFullname" (dict "root" . "name" "kafka")) -}}
{{- else -}}
{{- join "," .Values.messaging.kafka.bootstrap -}}
{{- end -}}
{{- end }}

{{- define "fastmock.accessEnabled" -}}
{{- $access := .Values.access | default dict -}}
{{- if or $access.roles $access.spaces $access.bindings -}}true{{- end -}}
{{- end -}}

{{- define "fastmock.accessFile" -}}
{{- $access := .Values.access | default dict -}}
{{- toYaml (dict "roles" ($access.roles | default list) "spaces" ($access.spaces | default list) "bindings" ($access.bindings | default list)) -}}
{{- end -}}
