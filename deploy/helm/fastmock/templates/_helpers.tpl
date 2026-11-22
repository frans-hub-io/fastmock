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

{{- define "fastmock.mailListeners" -}}
{{- $out := list -}}
{{- $mail := .Values.mail | default dict -}}
{{- if $mail.enabled -}}
{{- $ports := $mail.ports | default dict -}}
{{- range $l := list (list "smtp" "SMTP" false) (list "smtps" "SMTPS" true) (list "imap" "IMAP" false) (list "imaps" "IMAPS" true) (list "pop3" "POP3" false) (list "pop3s" "POP3S" true) -}}
{{- $port := index $ports (index $l 0) -}}
{{- if $port -}}
{{- $out = append $out (dict "name" (index $l 0) "port" ($port | int) "var" (printf "FASTMOCK_MAIL_%s_LISTEN" (index $l 1)) "tls" (index $l 2)) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- toJson $out -}}
{{- end -}}

{{- define "fastmock.mailValidate" -}}
{{- $mail := .Values.mail | default dict -}}
{{- if $mail.enabled -}}
{{- $seen := dict "8080" "the data plane (8080)" "9090" "the admin API (9090)" -}}
{{- range $l := include "fastmock.mailListeners" . | fromJsonArray -}}
{{- $key := toString (int $l.port) -}}
{{- if hasKey $seen $key -}}
{{- fail (printf "mail.ports.%s (%s) uses the same port as %s" $l.name $key (get $seen $key)) -}}
{{- end -}}
{{- $_ := set $seen $key (printf "mail.ports.%s" $l.name) -}}
{{- if and $l.tls (not (dig "tls" "existingSecret" "" $mail)) -}}
{{- fail (printf "mail.ports.%s needs a certificate: set mail.tls.existingSecret" $l.name) -}}
{{- end -}}
{{- end -}}
{{- $limits := $mail.limits | default dict -}}
{{- if and (hasKey $limits "maxConnections") (hasKey $limits "maxConnectionsPerIp") (gt (int $limits.maxConnectionsPerIp) (int $limits.maxConnections)) -}}
{{- fail "mail.limits.maxConnectionsPerIp must not exceed mail.limits.maxConnections" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "fastmock.captureSourcesEnabled" -}}
{{- $capture := .Values.messaging.capture -}}
{{- if and $capture.enabled $capture.sources -}}true{{- end -}}
{{- end -}}

{{- define "fastmock.captureSourcesFile" -}}
{{- toPrettyJson .Values.messaging.capture.sources -}}
{{- end -}}

{{- define "fastmock.brokersValidate" -}}
{{- $mqtt := .Values.messaging.mqtt -}}
{{- if $mqtt.url -}}
{{- if and $mqtt.existingSecret (not $mqtt.username) -}}
{{- fail "messaging.mqtt.username is required when messaging.mqtt.existingSecret is set" -}}
{{- end -}}
{{- if and (or $mqtt.tls.caSecret $mqtt.tls.clientCertSecret) (not (hasPrefix "mqtts://" $mqtt.url)) -}}
{{- fail "messaging.mqtt.tls needs an mqtts:// URL in messaging.mqtt.url" -}}
{{- end -}}
{{- else if or $mqtt.username $mqtt.existingSecret $mqtt.tls.caSecret $mqtt.tls.clientCertSecret $mqtt.allowedTopics -}}
{{- fail "messaging.mqtt.url is required when other messaging.mqtt settings are set" -}}
{{- end -}}
{{- $amqp := .Values.messaging.amqp -}}
{{- if $amqp.url -}}
{{- $tls := hasPrefix "amqps://" $amqp.url -}}
{{- if and (or $amqp.tls.caSecret $amqp.tls.clientCertSecret) (not $tls) -}}
{{- fail "messaging.amqp.tls needs an amqps:// URL in messaging.amqp.url" -}}
{{- end -}}
{{- if eq (upper ($amqp.sasl.mechanism | default "PLAIN")) "EXTERNAL" -}}
{{- if or (not $tls) (not $amqp.tls.clientCertSecret) -}}
{{- fail "messaging.amqp.sasl.mechanism=EXTERNAL needs an amqps:// URL and messaging.amqp.tls.clientCertSecret" -}}
{{- end -}}
{{- if or $amqp.sasl.username $amqp.sasl.existingSecret -}}
{{- fail "messaging.amqp.sasl.mechanism=EXTERNAL takes no username or existingSecret" -}}
{{- end -}}
{{- else if and $amqp.sasl.username (not $amqp.sasl.existingSecret) -}}
{{- fail "messaging.amqp.sasl.existingSecret is required when messaging.amqp.sasl.username is set" -}}
{{- else if and $amqp.sasl.existingSecret (not $amqp.sasl.username) -}}
{{- fail "messaging.amqp.sasl.username is required when messaging.amqp.sasl.existingSecret is set" -}}
{{- end -}}
{{- else if or $amqp.sasl.mechanism $amqp.sasl.username $amqp.sasl.existingSecret $amqp.tls.caSecret $amqp.tls.clientCertSecret $amqp.allowedExchanges $amqp.declareExchanges -}}
{{- fail "messaging.amqp.url is required when other messaging.amqp settings are set" -}}
{{- end -}}
{{- $capture := .Values.messaging.capture -}}
{{- if and $capture.sources (not $capture.enabled) -}}
{{- fail "messaging.capture.enabled must be true when messaging.capture.sources is set" -}}
{{- end -}}
{{- if $capture.enabled -}}
{{- $max := dig "limits" "maxSources" 50 $capture | int -}}
{{- if gt (len ($capture.sources | default list)) $max -}}
{{- fail (printf "messaging.capture.sources has %d sources but messaging.capture.limits.maxSources is %d" (len $capture.sources) $max) -}}
{{- end -}}
{{- $seen := dict -}}
{{- range $s := $capture.sources | default list -}}
{{- $id := printf "%s/%s" ($s.space | default "") $s.name -}}
{{- if hasKey $seen $id -}}
{{- fail (printf "messaging.capture.sources defines %s twice in the same space" $s.name) -}}
{{- end -}}
{{- $_ := set $seen $id true -}}
{{- if and (ne $s.protocol "amqp") (not $s.topics) -}}
{{- fail (printf "messaging.capture.sources %s: a %s source needs topics" $s.name $s.protocol) -}}
{{- end -}}
{{- if and (eq $s.protocol "amqp") (not $s.exchange) -}}
{{- fail (printf "messaging.capture.sources %s: an amqp source needs an exchange" $s.name) -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
