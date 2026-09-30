{{/*
Omnion Helm chart helpers (REQ-128, slice 2b).

Every template funnels through here for the names, so a rename is one edit rather than the
twenty that a hard-coded `omnion-api` scattered across the templates would need. The failure that
motivates the shared label set is specific: a chart whose pods carry two different
`app.kubernetes.io/name` values cannot be found by one `kubectl get pods -l` during an incident,
and the label that a selector does not match is a rollout that never schedules.
*/}}

{{- define "omnion.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "omnion.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "omnion.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
The selector labels. `include`d rather than written inline in every Deployment selector because a
selector is IMMUTABLE after apply: if this template changes and an existing Deployment still
carries the old selector, `helm upgrade` fails with a field-immutable error that names nothing
useful. Changing the chart's label scheme is therefore a breaking change, and the comment is here
so the next person knows before they find out from an upgrade.
*/}}
{{- define "omnion.labels" -}}
helm.sh/chart: {{ include "omnion.chart" . }}
{{ include "omnion.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: omnion
{{- end -}}

{{- define "omnion.selectorLabels" -}}
app.kubernetes.io/name: {{ include "omnion.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
Per-component labels. The component is in the selector and in the pod template, which is what
lets one HPA or one PDB address the API without touching the renderer.
*/}}
{{- define "omnion.componentLabels" -}}
{{- $component := index . 0 -}}
{{- $root := index . 1 -}}
helm.sh/chart: {{ include "omnion.chart" $root }}
{{ include "omnion.selectorLabels" $root }}
app.kubernetes.io/component: {{ $component }}
app.kubernetes.io/version: {{ $root.Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ $root.Release.Service }}
app.kubernetes.io/part-of: omnion
{{- end -}}

{{- define "omnion.componentSelectorLabels" -}}
{{- $component := index . 0 -}}
{{- $root := index . 1 -}}
app.kubernetes.io/name: {{ include "omnion.name" $root }}
app.kubernetes.io/instance: {{ $root.Release.Name }}
app.kubernetes.io/component: {{ $component }}
{{- end -}}

{{/*
The image reference.

`digest` wins over `tag`, and that precedence is deliberate rather than incidental: a tag is a
mutable name that anybody with push access can re-point, so a deployment that pins a tag can be
rolled onto different bytes without a single value changing. `imagePullPolicy: IfNotPresent`
combined with a mutable tag is the same class of problem, which is why the digest form is
documented as the reproducible one.
*/}}
{{- define "omnion.image" -}}
{{/* The caller passes `dict "root" $ctx "component" $name`, so the values live under `.root`. */}}
{{- $root := .root -}}
{{- $component := .component | default "api" -}}
{{/* The values block a component's image overrides live under, and the image NAME the pull
     reference is built from. They differ for exactly one component: the migration Job runs
     `--migrate-only`, a MODE OF THE API BINARY, so it must PULL THE API IMAGE. Keying the name on
     the component would ask for `omnion-migrate`, which no Dockerfile builds and no release
     publishes — a pull failure at `pre-install`, the worst possible moment to discover it. The
     OVERRIDE still lives under `migration.image`, so an operator who genuinely needs a different
     build for the job has somewhere to say so. */}}
{{- $imageKey := $component -}}
{{- $imageSuffix := $component -}}
{{- if eq $component "migrate" -}}
  {{- $imageKey = "migration" -}}
  {{- $imageSuffix = "api" -}}
{{- end -}}
{{/* `index` on a map with a missing key returns nil, and `kindIs` on nil is a template error, so
     the lookup is guarded with `hasKey` rather than defended after the fact. */}}
{{- $spec := dict -}}
{{- if and $root.Values (hasKey $root.Values $imageKey) -}}
  {{- $spec = index $root.Values $imageKey -}}
{{- end -}}
{{- $img := $root.Values.image | default dict -}}
{{- if and $spec (kindIs "map" $spec) $spec.image -}}
  {{/* A component override is merged, but an EMPTY override key must not REPLACE the release
       value with a blank: `image.tag: ""` in the job's block would otherwise make the tag fall
       through to `appVersion` while the pods took --set, which is a migration running different
       code from the release it is migrating. Empty means "inherit", by construction.
       The restore has to copy the BASE value back: `unset`ing the key instead produces the very
       same fall-through it was written to prevent, one indirection further from the cause. */}}
  {{- $override := $spec.image | default dict -}}
  {{- $base := deepCopy $img -}}
  {{- $merged := mergeOverwrite (deepCopy $img) $override -}}
  {{- range $k, $v := $override -}}
    {{- if not $v -}}
      {{- if hasKey $base $k -}}
        {{- $_ := set $merged $k (get $base $k) -}}
      {{- else -}}
        {{- $_ := unset $merged $k -}}
      {{- end -}}
    {{- end -}}
  {{- end -}}
  {{- $img = $merged -}}
{{- end -}}
{{- $registry := $img.registry | default "" -}}
{{- $repository := $img.repository | default "raksix/omnion" -}}
{{- $name := printf "%s-%s" $repository $imageSuffix -}}
{{- $digest := $img.digest | default "" -}}
{{- if $digest -}}
{{- if not (hasPrefix "sha256:" $digest) -}}
{{- fail (printf "image.digest must start with sha256: (got %q)" $digest) -}}
{{- end -}}
{{- if $registry -}}
{{- printf "%s/%s@%s" $registry $name $digest -}}
{{- else -}}
{{- printf "%s@%s" $name $digest -}}
{{- end -}}
{{- else -}}
{{- $tag := $img.tag | default $root.Chart.AppVersion | toString -}}
{{- if $registry -}}
{{- printf "%s/%s:%s" $registry $name $tag -}}
{{- else -}}
{{- printf "%s:%s" $name $tag -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
A secret VALUE, read from an existing Secret the operator created.

Two rules, both learned the hard way:
  1. This returns a `secretKeyRef`, never the value. There is deliberately no template that can
     print a credential, so `helm template … | grep` can be used as a leak assertion (and the
     gate does exactly that).
  2. `required: true` turns a missing Secret into a template failure instead of a pod that starts
     and crash-loops with an empty password. A failure at `helm install` names the chart; the same
     failure at runtime names a pod in a CrashLoopBackOff with no reason in it.
*/}}
{{- define "omnion.secretKeyRef" -}}
{{- $key := .key -}}
{{- $name := .name -}}
{{- $required := .required | default true -}}
{{- if $required }}
{{- if not $name }}
{{- fail (printf "a required credential (%s) has no existingSecret; create the Secret or set secrets.required=false" $key) }}
{{- end }}
{{- end }}
{{- if $name }}
secretKeyRef:
  name: {{ $name }}
  key: {{ $key }}
{{- end }}
{{- end -}}

{{/*
Resolve one credential to either a literal value (an endpoint, which is not a secret) or a
`secretKeyRef` (a password, which is). Keeping the two apart in one helper is what makes the
"references only" acceptance line checkable by a single grep.
*/}}
{{- define "omnion.endpointValue" -}}
{{- $root := .root -}}
{{- $literal := .literal | default "" -}}
{{- $existing := .existing | default "" -}}
{{- $defaultKey := .defaultSecretKey | default "" -}}
{{- if $literal -}}
{{- $literal | quote -}}
{{- else if $existing -}}
{{- $required := $root.Values.secrets.required | default true -}}
{{- include "omnion.secretKeyRef" (dict "key" $defaultKey "name" $existing "required" $required) -}}
{{- end -}}
{{- end -}}

{{- define "omnion.imagePullSecrets" -}}
{{- with .Values.imagePullSecrets }}
imagePullSecrets:
{{ toYaml . }}
{{- end }}
{{- end -}}

{{- define "omnion.serviceAccountName" -}}
{{- default (include "omnion.fullname" .) .Values.serviceAccount.name | default (include "omnion.fullname" .) -}}
{{- end -}}

{{/*
`helm.sh/hook-delete-policy: before-hook-creation,hook-succeeded`.

`before-hook-creation` alone would leave every previous run's Job object in the namespace, and a
failed migration would then be a hook-delete-policy question instead of a log question. Keeping
the succeeded run is useful, keeping the failed one is not, so the policy is per-outcome rather
than a blanket delete.
*/}}
{{- define "omnion.migrationHookAnnotations" -}}
"helm.sh/hook": pre-install,pre-upgrade
"helm.sh/hook-weight": {{ .Values.migration.hookWeight | quote }}
"helm.sh/hook-delete-policy": before-hook-creation,hook-succeeded
{{- end -}}
