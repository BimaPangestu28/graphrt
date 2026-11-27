{{- define "graphrt-collab.name" -}}{{ .Release.Name }}-graphrt-collab{{- end -}}
{{- define "graphrt-collab.labels" -}}
app.kubernetes.io/name: graphrt-collab
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
