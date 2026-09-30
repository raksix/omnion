{{/*
The post-install text. It lives in a `define` rather than in NOTES.txt itself so the release
gate can RENDER it without a cluster: NOTES.txt is only emitted by `helm install`/`upgrade`,
both of which dial the API server even with `--dry-run` (verified: `--dry-run` is documented as
"will not attempt cluster connections" and it connects anyway). A define can be rendered through
`helm template` by a probe template, so the text an operator sees in their terminal is the text
the gate checked — rather than a second copy that drifts from it.
*/}}
{{- define "omnion.notes" -}}
Omnion {{ .Chart.AppVersion }} is installed as release {{ .Release.Name }} in namespace {{ .Release.Namespace }}.

Readiness is the gate, not "the pods exist": the API answers /healthz as soon as the process is
up and /readyz only once PostgreSQL and Redis answer. Watch the second one come green.

  kubectl --namespace {{ .Release.Namespace }} rollout status deployment/{{ include "omnion.fullname" . }}-api
  kubectl --namespace {{ .Release.Namespace }} get pods -l app.kubernetes.io/instance={{ .Release.Name }}

1. Open {{ .Values.publicUrl }} (renderer) and the admin host from `ingress.hosts`.

2. Bootstrap the first administrator. The compose stack reads OMNION_ADMIN_EMAIL /
   OMNION_ADMIN_PASSWORD from its environment; this chart takes them from the Secret
   {{ .Values.secrets.existingSecret | default "(none configured)" }}, so create that Secret first
   or install with --set secrets.required=false and set the keys afterwards:

     kubectl --namespace {{ .Release.Namespace }} create secret generic {{ .Values.secrets.existingSecret }} \
       --from-literal={{ .Values.secrets.keys.databaseUrl }}=postgres://… \
       --from-literal={{ .Values.secrets.keys.redisUrl }}=redis://… \
       --from-literal={{ .Values.secrets.keys.s3AccessKey }}=… \
       --from-literal={{ .Values.secrets.keys.s3SecretKey }}=… \
       --from-literal={{ .Values.secrets.keys.csrfSecret }}=…

   Then open the panel and finish the setup wizard.

{{- if .Values.migration.enabled }}

Migrations ran as a pre-install/pre-upgrade hook, so this release is already on the new schema.
The hook Job is deleted on success; to read what it did:

  kubectl --namespace {{ .Release.Namespace }} get events --sort-by=.lastTimestamp | grep migrate
{{- end }}

{{- if not .Values.postgresql.dsn }}
The chart ships NO database. Set postgresql.dsn (or postgresql.existingSecret) to the PostgreSQL
the organisation already runs before the API pods can serve.
{{- end }}

Nothing above prints a credential, and nothing in this chart can: every password is a
secretKeyRef to a Secret you created.
{{- end }}
