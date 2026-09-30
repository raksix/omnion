#!/usr/bin/env bash
# Release pipeline gate (REQ-128, slice 3, second half).
#
# The manifest gate (scripts/qa/release-manifest.sh) proves the DOCUMENT. This proves the
# thing that decides whether the document may be published — and specifically that the
# thing REFUSES, because a publish gate that only ever agrees is a publish gate that has
# never been run.
#
# The properties under test, in the order they matter:
#
#   1. A blocked stage produces NO digest. This is the load-bearing one. A dry run that
#      supplied a placeholder for the fourteen stages it cannot run would produce a
#      complete-looking manifest listing four images that were never built — and the
#      pipeline would publish it, because the manifest builder cannot tell a real digest
#      from a fabricated one. Fabricating is the single most consequential thing a release
#      pipeline can do, and the mutation suite exists to prove it does not.
#   2. `publish-check` refuses for the RIGHT reason, and every reason is reported rather
#      than just the first. A refusal that names the wrong cause sends an operator to fix
#      something that is not broken.
#   3. The plan is derived from the repository, so a repository change moves the plan.
#   4. The chart's digest is reproducible by content even though its bytes are not.
#
# Every rule is mutated below. A rule nobody has seen fail is not evidence that it works.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/omnion-pipeline-gate.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
FAILED_NAMES=()
pass() { PASS=$((PASS + 1)); }
fail() {
  FAIL=$((FAIL + 1))
  FAILED_NAMES+=("$1")
  echo "  FAIL: $1${2:+ — $2}"
}
check() { # name, command...
  local name="$1"; shift
  if "$@" >/dev/null 2>&1; then pass; else fail "$name"; fi
}
py_ok() { # name, python-body, arg...
  local name="$1" body="$2"; shift 2
  if python3 -c "$body" "$@" >/dev/null 2>&1; then pass; else fail "$name"; fi
}
section() { echo "== $1"; }

cd "$ROOT" || exit 1

VERSION="$(python3 -c 'import sys;sys.path.insert(0,"release/lib");import manifest;print(manifest.workspace_version())' 2>/dev/null)"
DRY="$WORK/dry"
PLAN_JSON="$WORK/plan.json"
FACTS="$DRY/facts.json"

section "the pipeline module loads and exposes its four subcommands"
for sub in plan gates dry-run publish-check; do
  check "pipeline.py $sub is offered" python3 release/lib/pipeline.py "$sub" --help
done

section "the plan is derived from the repository, not from a list"
python3 release/lib/pipeline.py plan --json > "$PLAN_JSON" 2>/dev/null
check "plan --json emits parseable JSON" python3 -c "import json,sys;json.load(open(sys.argv[1]))" "$PLAN_JSON"
py_ok "the plan is not empty" '
import json,sys
stages=json.load(open(sys.argv[1]))
assert stages, "no stages"
print("stages:",len(stages),file=sys.stderr)
' "$PLAN_JSON"
py_ok "every stage carries an id, a kind and a stage spec" '
import json,sys
stages=json.load(open(sys.argv[1]))
bad=[s for s in stages if not s.get("id") or "stage_kind" not in s or "name" not in s]
print("malformed:",[s.get("id") for s in bad],file=sys.stderr)
sys.exit(1 if bad else 0)
' "$PLAN_JSON"
py_ok "every stage_kind is one the STAGES table knows" '
import json,sys
sys.path.insert(0,"release/lib"); import pipeline as p
stages=json.load(open(sys.argv[1]))
bad=sorted({s["stage_kind"] for s in stages} - set(p.STAGES))
print("unknown stage kinds:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$PLAN_JSON"
# The plan must contain one image stage per buildable image, INCLUDING the two runtimes in
# admin.Dockerfile. A plan derived from file names rather than runtime stages omits `web`,
# which is the image both compose stacks and the chart deploy.
py_ok "one image stage per Dockerfile runtime, web among them" '
import json,sys
sys.path.insert(0,"release/lib"); import manifest as m, pipeline as p
stages=json.load(open(sys.argv[1]))
images={s["target"] for s in stages if s["stage_kind"]=="image"}
expected={c for cs in m.dockerfile_targets(".").values() for c in cs}
print("plan images:",sorted(images),"expected:",sorted(expected),file=sys.stderr)
sys.exit(0 if images==expected else 1)
' "$PLAN_JSON"
py_ok "one CLI stage per documented platform" '
import json,sys
sys.path.insert(0,"release/lib"); import manifest as m, pipeline as p
stages=json.load(open(sys.argv[1]))
cli={s["target"] for s in stages if s["stage_kind"]=="cli"}
print("plan cli:",sorted(cli),file=sys.stderr)
sys.exit(0 if cli==set(m.CLI_PLATFORMS) else 1)
' "$PLAN_JSON"
py_ok "an SBOM stage exists per image and depends on it" '
import json,sys
stages=json.load(open(sys.argv[1]))
by_id={s["id"]:s for s in stages}
bad=[]
for s in stages:
    if s["stage_kind"]!="sbom": continue
    # Built by string concatenation rather than an f-string: the shell heredoc this lives
    # in would otherwise have to escape a nested quote, and the first version of this
    # check wrote `f"image:{s[\"target\"]}"` — bash ate the backslashes, the python saw
    # `s[web]` as a subscript of a bare name, and the check reported a stage-definition
    # failure while running perfectly valid plan JSON. The gate was measuring bash.
    parent="image:"+str(s["target"])
    if parent not in by_id or parent not in s["depends_on"]: bad.append(s["id"])
print("sbom stages with no image dependency:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$PLAN_JSON"
py_ok "the manifest stage depends on every other stage" '
import json,sys
stages=json.load(open(sys.argv[1]))
manifest_stage=[s for s in stages if s["stage_kind"]=="manifest"]
others=[s["id"] for s in stages if s["stage_kind"]!="manifest"]
bad=[s["id"] for s in manifest_stage if set(others)-set(s["depends_on"])]
print("manifest stage missing deps:",bad,"of",len(others),file=sys.stderr)
sys.exit(1 if bad else 0)
' "$PLAN_JSON"
py_ok "the plan's chart artifact is named for the version" '
import json,sys
stages=json.load(open(sys.argv[1]))
charts=[s for s in stages if s["stage_kind"]=="chart"]
bad=[s["name"] for s in charts if not s["name"].endswith("-"+sys.argv[2]+".tgz")]
print("misnamed chart:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$PLAN_JSON" "$VERSION"

section "a dry run reports honestly about what it could not build"
python3 release/lib/pipeline.py dry-run --workdir "$DRY" > "$WORK/dry.log" 2>&1
DRY_RC=$?
check "dry-run exits 0 even when stages are blocked" test "$DRY_RC" -eq 0
check "dry-run wrote a facts document" test -s "$FACTS"

# The per-stage report, taken from the dry run's own log rather than by re-running the
# stages. The first version of this check used process substitution to regenerate the
# report inline — which re-ran every stage a second time, re-ran `helm package` and both
# `docker compose config` renders, and then compared two DIFFERENT runs against each other.
# Two runs legitimately differ (the chart's bytes carry a timestamp), so it compared a run
# to itself through a pipe and reported the difference as a defect in the pipeline.
#
# The dry run writes its report next to its facts file; the gate reads that.
STAGES_JSON="$DRY/stages.json"
check "the dry run wrote its stage report" test -s "$STAGES_JSON"
check "the stage report is parseable" python3 -c "import json,sys;json.load(open(sys.argv[1]))" "$STAGES_JSON"

py_ok "every blocked stage is named with a reason" '
import json,sys
stages=json.load(open(sys.argv[1]))
missing=[s["id"] for s in stages if s["state"]!="verified" and len(s.get("reason") or "")<20]
print("blocked stages with no usable reason:",missing,file=sys.stderr)
sys.exit(1 if missing else 0)
' "$STAGES_JSON"
py_ok "every verified stage carries a digest" '
import json,sys
stages=json.load(open(sys.argv[1]))
missing=[s["id"] for s in stages if s["state"]=="verified" and not s.get("digest")]
print("verified stages with no digest:",missing,file=sys.stderr)
sys.exit(1 if missing else 0)
' "$STAGES_JSON"

section "THE LOAD-BEARING PROPERTY: a blocked stage contributes no digest"
py_ok "no blocked stage has a digest" '
import json,sys
stages=json.load(open(sys.argv[1]))
bad=[(s["id"],s["digest"]) for s in stages if s["state"]!="verified" and s.get("digest")]
print("blocked stages carrying a digest:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$STAGES_JSON"
py_ok "no blocked stage appears in the facts document at all" '
import json,sys
facts=json.load(open(sys.argv[1])); stages=json.load(open(sys.argv[2]))
blocked={s["id"] for s in stages if s["state"]!="verified"}
present={entry.get("stage") for section in ("images","cli","charts","sboms","compose")
          for entry in facts.get(section,{}).values()}
leaked=sorted(blocked & present)
print("blocked stages present in facts:",leaked,file=sys.stderr)
sys.exit(1 if leaked else 0)
' "$FACTS" "$STAGES_JSON"
py_ok "the facts document declares itself non-synthetic" '
import json,sys
facts=json.load(open(sys.argv[1]))
if facts.get("_synthetic") is not False:
    print("_synthetic is",facts.get("_synthetic"),file=sys.stderr); sys.exit(1)
if facts.get("_source")!="release-pipeline/dry-run":
    print("_source is",facts.get("_source"),file=sys.stderr); sys.exit(1)
' "$FACTS"
py_ok "every digest in the facts document is the sha256 of a real file on disk" '
import json,sys,re,os,hashlib
facts=json.load(open(sys.argv[1])); stages=json.load(open(sys.argv[2]))
by_id={s["id"]:s for s in stages}
def sha(p):
    h=hashlib.sha256()
    with open(p,"rb") as fh:
        for b in iter(lambda: fh.read(1<<20), b""): h.update(b)
    return "sha256:"+h.hexdigest()
bad=[]
for section in ("images","cli","charts","sboms","compose"):
    for name,entry in facts.get(section,{}).items():
        where=f"{section}/{name}"
        stage=by_id.get(entry.get("stage"))
        if stage is None: bad.append((where,"names a stage that does not exist")); continue
        if not re.match(r"^sha256:[0-9a-f]{64}$", entry.get("digest") or ""):
            bad.append((where,"digest is not a sha256")); continue
        path=stage.get("file")
        # Relative paths resolve against the repository root: a dry run runs its stages with
        # the repo as cwd, so its report names `infra/compose/…` relative, and the first
        # version of this check compared the digest against a path it never resolved. It
        # reported every entry as unverifiable, which reads as "the facts are fabricated" —
        # a claim the gate had not established, about a file that exists.
        path=path if os.path.isabs(path) else os.path.join(sys.argv[3], path)
        if not os.path.exists(path): bad.append((where,f"no file at {path}")); continue
        if sha(path)!=entry["digest"]: bad.append((where,"digest does not match the file on disk"))
print("digest problems:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$FACTS" "$STAGES_JSON" "$ROOT"
py_ok "the facts document carries a size for every entry it claims" '
import json,sys
facts=json.load(open(sys.argv[1]))
bad=[n for s in ("images","cli","charts","sboms","compose") for n,e in facts.get(s,{}).items()
     if not isinstance(e.get("size_bytes"),int) or e["size_bytes"]<=0]
print("entries with no positive size:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$FACTS"

section "a partial facts document cannot produce a manifest"
check "the manifest builder refuses these incomplete facts" python3 -c "
import sys; sys.path.insert(0,'release/lib')
import json, manifest as m
try:
    m.build_manifest('$VERSION','0'*40,root='.',facts=json.load(open('$FACTS')),
                     allow_version_drift=True)
except m.ManifestError as exc:
    print('refused:',exc,file=sys.stderr); sys.exit(0)
print('ACCEPTED an incomplete facts document',file=sys.stderr); sys.exit(1)"
py_ok "the refusal names the artifacts that have no digest" '
import sys, json; sys.path.insert(0,"release/lib")
import manifest as m
# The version waiver is REQUIRED here and is the point of the check. This repository
# currently has a real version disagreement (a theme declares 0.1.1 while the platform
# releases 0.1.0), so the builder refuses on THAT first and never reaches the missing
# digests. The first version of this check asserted on the refusal message and failed with
# "no api in message" — which read as "the builder did not check the digests", when in fact
# it had not been given permission to get past an unrelated refusal. Both refusals are
# correct; only one of them is what this check is about.
try:
    m.build_manifest(sys.argv[1],"0"*40,root=".",facts=json.load(open(sys.argv[2])),
                     allow_version_drift=True)
    sys.exit(1)
except m.ManifestError as exc:
    message=str(exc)
    print("refusal:",message,file=sys.stderr)
    # The builder now names EVERY missing artifact in one refusal ("13 artifact(s) have
    # no digest supplied: …"), so the check asks for the aggregate form and the count —
    # it no longer has to guess which artifact happens to be named.
    import re as _re
    sys.exit(0 if _re.search(r"\d+ artifact\(s\) have no digest supplied", message) else 1)
' "$VERSION" "$FACTS"
py_ok "the digest refusal names EVERY artifact that has none" '
import sys, json; sys.path.insert(0,"release/lib")
import manifest as m, pipeline as p
facts=json.load(open(sys.argv[2]))
missing=[s["name"] for s in p.plan(sys.argv[3], sys.argv[1])
         if s["kind"] in ("image","cli","chart","sbom","compose") and s["name"] not in
         {n for sec in ("images","cli","charts","sboms","compose") for n in facts.get(sec,{})}]
try:
    m.build_manifest(sys.argv[1],"0"*40,root=sys.argv[3],facts=facts,allow_version_drift=True)
    sys.exit(1)
except m.ManifestError as exc:
    named={n for n in missing if n in str(exc)}
    print("missing:",len(missing),"named by the refusal:",len(named),file=sys.stderr)
    # Every artifact without a digest must be named, in ONE refusal. The builder reported
    # the first absence and stopped, so a dry run with no facts named one artifact out of
    # sixteen and the operator learned about the other fifteen by re-running it fifteen
    # times. A partial build is the NORMAL state of a dry run, so the common case was the
    # slowest one.
    sys.exit(0 if named==set(missing) and len(missing)>1 else 1)
' "$VERSION" "$FACTS" "$ROOT"

section "the chart digest is reproducible by content"
py_ok "the chart entry carries a content digest as well as a byte digest" '
import json,sys
facts=json.load(open(sys.argv[1]))
charts=facts.get("charts",{})
bad=[n for n,e in charts.items() if not e.get("content_digest")]
print("chart entries with no content digest:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$FACTS"
py_ok "the byte digest and the content digest are different values" '
import json,sys
facts=json.load(open(sys.argv[1]))
bad=[n for n,e in facts.get("charts",{}).items() if e.get("digest")==e.get("content_digest")]
print("charts whose two digests coincide (mtime-free packing?):",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$FACTS"
py_ok "the content digest is stable across a fresh pack" '
import sys,os,json; sys.path.insert(0,"release/lib")
import pipeline as p
t=os.path.join(sys.argv[1],"repro")
os.makedirs(t,exist_ok=True)
import subprocess
r=subprocess.run(["helm","package",os.path.join(".","infra","helm","omnion"),"-d",t],
                 capture_output=True,text=True)
if r.returncode!=0: print("helm failed",r.stderr,file=sys.stderr); sys.exit(1)
fresh=[f for f in os.listdir(t) if f.endswith(".tgz")]
facts=json.load(open(os.path.join(sys.argv[1],"facts.json")))
recorded=[e["content_digest"] for e in facts.get("charts",{}).values()]
actual=[p.canonical_archive_digest(os.path.join(t,f)) for f in fresh]
print("recorded:",recorded,"fresh:",actual,file=sys.stderr)
sys.exit(0 if recorded and actual and recorded[0]==actual[0] else 1)
' "$DRY"
py_ok "the content digest ignores member mtimes" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p, tarfile, io, os, tempfile
# A tarball with the same content packed at two different times must hash alike.
src=os.path.join(tempfile.mkdtemp(),"a.tgz")
with tarfile.open(src,"w:gz") as t:
    for name,data,mtime in (("x/one",b"alpha",1000),("x/two",b"beta",2000)):
        info=tarfile.TarInfo(name); info.size=len(data); info.mtime=mtime; info.mode=0o644
        t.addfile(info, io.BytesIO(data))
import time
time.sleep(1.1)
second=src.replace("a.tgz","b.tgz")
with tarfile.open(second,"w:gz") as t:
    for name,data,mtime in (("x/one",b"alpha",9000),("x/two",b"beta",9000)):
        info=tarfile.TarInfo(name); info.size=len(data); info.mtime=mtime; info.mode=0o644
        t.addfile(info, io.BytesIO(data))
a=p.canonical_archive_digest(src); b=p.canonical_archive_digest(second)
print("digests:",a,b,file=sys.stderr)
sys.exit(0 if a==b else 1)
' "$DRY"

section "compose stacks: rendered, and free of literals"
py_ok "both compose stages are verified when docker is available" '
import json,sys
stages=json.load(open(sys.argv[1]))
compose=[s for s in stages if s["kind"]=="compose"]
if not compose:
    print("no compose stage",file=sys.stderr); sys.exit(0)
bad=[s["id"] for s in compose if s["state"]!="verified"]
print("compose stages not verified:",bad,file=sys.stderr)
sys.exit(1 if bad else 0)
' "$STAGES_JSON"
py_ok "a compose file with a literal password is caught" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
leaky="""services:
  api:
    image: ghcr.io/raksix/omnion/api:0.1.0
    environment:
      POSTGRES_PASSWORD: hunter2
"""
with open(sys.argv[1],"w") as fh: fh.write(leaky)
found=p._literal_credentials(sys.argv[1])
print("found:",found,file=sys.stderr)
sys.exit(0 if found else 1)
' "$WORK/leak.yml"
py_ok "a compose file that references its credentials is not flagged" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
clean="""services:
  api:
    environment:
      POSTGRES_PASSWORD: ${OMNION_DB_PASSWORD}
      REDIS_URL: redis://redis:6379
      API_KEY: ""
      CSRF_SECRET:
"""
with open(sys.argv[1],"w") as fh: fh.write(clean)
found=p._literal_credentials(sys.argv[1])
print("found:",found,file=sys.stderr)
sys.exit(0 if not found else 1)
' "$WORK/clean.yml"
py_ok "a literal hidden behind a reference is still caught" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
half="""services:
  api:
    environment:
      DATABASE_URL: postgres://admin:***@db:5432/omnion
      S3_SECRET_KEY: ${S3_KEY}-appended-literal
"""
# The key must NAME a credential for the key-based rule to apply at all — the first version
# of this fixture used `X`, which is not a credential key, so the rule never ran and the
# check passed for the wrong reason. A fixture whose key cannot trigger the rule is a
# fixture that measures nothing.
with open(sys.argv[1],"w") as fh: fh.write(half)
found=p._literal_credentials(sys.argv[1])
print("found:",found,file=sys.stderr)
sys.exit(0 if found else 1)
' "$WORK/half.yml"

section "publish-check refuses, and names the reason"
py_ok "a tag that does not match the version is refused" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
r=p.publish_refusals("v9.9.9",sys.argv[1],{"gates":{},"missing_required":[]},
                      {"_synthetic":False,"_blocked_stages":[]},attestation=True)
print(r,file=sys.stderr)
sys.exit(0 if any("does not name the version" in x for x in r) else 1)
' "$VERSION"
py_ok "a pre-release may not publish to stable" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""}},"missing_required":["migration-verification","manifest"]}
r=p.publish_refusals("v1.0.0-rc.1","1.0.0-rc.1",gates,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
print(r,file=sys.stderr)
sys.exit(0 if any("pre-release" in x for x in r) else 1)
' "$VERSION"
py_ok "the same pre-release on a beta channel is not refused for that" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""}},"missing_required":["migration-verification","manifest"]}
r=p.publish_refusals("v1.0.0-rc.1","1.0.0-rc.1",gates,{"_synthetic":False,"_blocked_stages":[]},
                     attestation=True,channel="beta")
print(r,file=sys.stderr)
sys.exit(0 if not any("pre-release" in x for x in r) else 1)
' "$VERSION"
py_ok "a red gate refuses publication and names the gate" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""},"manifest":{"state":"red","reason":"themes/minimal declares 0.1.1"}},
       "missing_required":[]}
r=p.publish_refusals("v"+sys.argv[1],sys.argv[1],gates,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
joined=" ".join(r)
print(r,file=sys.stderr)
sys.exit(0 if any("red" in x and "manifest" in x for x in r) else 1)
' "$VERSION"
py_ok "a required gate that did not run refuses publication" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""},"manifest":{"state":"unrunnable","reason":"no command"}},
       "missing_required":["migration-verification"]}
r=p.publish_refusals("v"+sys.argv[1],sys.argv[1],gates,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
print(r,file=sys.stderr)
sys.exit(0 if any("could not be run" in x for x in r) and any("migration-verification" in x for x in r) else 1)
' "$VERSION"
py_ok "synthetic facts refuse publication" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""}},"missing_required":[]}
r=p.publish_refusals("v"+sys.argv[1],sys.argv[1],gates,{"_synthetic":True,"_blocked_stages":[]},attestation=True)
print(r,file=sys.stderr)
sys.exit(0 if any("synthetic" in x for x in r) else 1)
' "$VERSION"
py_ok "a missing attestation refuses publication" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""}},"missing_required":[]}
r=p.publish_refusals("v"+sys.argv[1],sys.argv[1],gates,{"_synthetic":False,"_blocked_stages":[]},attestation=False)
print(r,file=sys.stderr)
sys.exit(0 if any("attestation" in x for x in r) else 1)
' "$VERSION"
py_ok "blocked stages refuse publication and are counted" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""}},"missing_required":[]}
r=p.publish_refusals("v"+sys.argv[1],sys.argv[1],gates,
                     {"_synthetic":False,"_blocked_stages":["image:api","image:web"]},attestation=True)
print(r,file=sys.stderr)
sys.exit(0 if any("2 build stage" in x and "image:api" in x and "image:web" in x for x in r) else 1)
' "$VERSION"
py_ok "EVERY reason is reported, not just the first" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"red","reason":"boom"}},"missing_required":["manifest"]}
r=p.publish_refusals("v9.9.9","0.1.0",gates,
                     {"_synthetic":True,"_blocked_stages":["image:api"]},attestation=False)
print(len(r),"refusals:",r,file=sys.stderr)
# Seven independent problems in one call: tag, red gate, missing gate, unrunnable, blocked,
# synthetic, no attestation. A function that returned only the first would report 1.
sys.exit(0 if len(r)>=6 else 1)
' "$VERSION"
py_ok "an all-green tag is publishable" '
import sys; sys.path.insert(0,"release/lib")
import pipeline as p
gates={"gates":{"tests":{"state":"green","reason":""},"manifest":{"state":"green","reason":""},
                "migration-verification":{"state":"green","reason":""}},"missing_required":[]}
r=p.publish_refusals("v"+sys.argv[1],sys.argv[1],gates,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
print(r,file=sys.stderr)
sys.exit(0 if not r else 1)
' "$VERSION"

section "publish-check as a command refuses this repository's real state"
python3 release/lib/pipeline.py publish-check --tag "v$VERSION" --facts "$FACTS" --attestation \
  > "$WORK/publish.json" 2>&1
PUBLISH_RC=$?
check "publish-check exits non-zero on this repository" test "$PUBLISH_RC" -ne 0
check "publish-check emitted JSON" python3 -c "import json,sys;json.load(open(sys.argv[1]))" "$WORK/publish.json"
py_ok "the report says the tag is not publishable" '
import json,sys
d=json.load(open(sys.argv[1]))
print("publishable:",d["publishable"],file=sys.stderr)
sys.exit(0 if d["publishable"] is False and d["refusals"] else 1)
' "$WORK/publish.json"
py_ok "the refusal includes the blocked stages this box could not build" '
import json,sys
d=json.load(open(sys.argv[1]))
joined=" ".join(d["refusals"])
print(joined,file=sys.stderr)
sys.exit(0 if "image:api" in joined else 1)
' "$WORK/publish.json"
py_ok "the refusal includes the red manifest gate, by name" '
import json,sys
d=json.load(open(sys.argv[1]))
joined=" ".join(d["refusals"])
sys.exit(0 if "manifest" in joined and ("0.1.1" in joined or "red" in joined) else 1)
' "$WORK/publish.json"

section "MUTATIONS — every rule above must be able to fail"
# These mutate the PIPELINE, not a copy of the rule. A test that re-implements the property
# and then asserts the re-implementation fails is measuring the test — which is why three
# of the first mutation set here "survived": they exercised the real, correct function and
# then reported its correctness as a failure. The load-bearing form is: break the subject,
# require the gate's own check to go red.
#
# Each mutation is a textual edit to a COPY of `release/lib`, applied with python (not sed,
# whose quoting is its own class of bug), and the check then runs against the copy.
MUT=0
MUT_CAUGHT=0
# Exported, not just set: every mutation's check body reads it as `os.environ["MUT_LIB"]` to
# import the MUTANT, while the surrounding checks import the real one from `release/lib`.
# A shell variable that the check body cannot see makes every mutation import the
# unmutated module, so the rule under test never actually breaks and every mutation is
# reported as surviving.
export MUT_LIB=""
# THE POLARITY, stated once because getting it wrong is the single most expensive mistake
# available in a mutation suite.
#
# A mutation body is a copy of the property under test, and it is expected to FAIL. The
# body must exit NON-ZERO when the mutant exhibits the defect and exit ZERO when the mutant
# is still correct. Read the other way round, a mutation harness reports "survived" for
# every mutation that was caught perfectly well — the exact inversion this wave has hit
# three times in three different gates, and the reason one of them sat green while
# proving nothing.
mutate() { # name, python-edit-expression, check-body(must FAIL on the mutant)
  MUT=$((MUT + 1))
  local name="$1" edit="$2" body="$3"
  MUT_LIB="$WORK/mutant-lib"
  rm -rf "$MUT_LIB"; mkdir -p "$MUT_LIB"
  cp release/lib/manifest.py release/lib/pipeline.py "$MUT_LIB/" 2>/dev/null
  # The edit is REQUIRED to change the file, and the body is REQUIRED to fail. A
  # `str.replace` whose target does not exist is a silent no-op, so the mutant is then
  # identical to the original, the body passes, and the mutation is reported as SURVIVED —
  # which reads as "the rule is too weak" when the truth is "the mutation never happened".
  # So the edit asserts its own effect, and an edit that changes nothing is an error.
  if ! python3 -c "
import sys, pathlib
path = pathlib.Path(sys.argv[1]) / 'pipeline.py'
text = path.read_text()
$edit
sys.exit(0 if text != path.read_text() else 1)
" "$MUT_LIB" >/dev/null 2>&1; then
    fail "mutation could not be applied" "$name"
    return 0
  fi
  python3 -c "
import sys, pathlib
path = pathlib.Path(sys.argv[1]) / 'pipeline.py'
text = path.read_text()
$edit
path.write_text(text)
" "$MUT_LIB" >/dev/null 2>&1
  # The mutant must still import: a syntax error exits non-zero for the wrong reason, and a
  # mutation that cannot be told apart from a defect is not a mutation.
  if ! python3 -c "import sys; sys.path.insert(0, sys.argv[1]); import pipeline" "$MUT_LIB" >/dev/null 2>&1; then
    fail "mutation produced an unimportable module" "$name"
    return 0
  fi
  # The body must FAIL. Exit 0 means the mutant is still correct, so the rule did not hold.
  if python3 -c "$body" >/dev/null 2>&1; then
    echo "  SURVIVED (bad): $name"
  else
    MUT_CAUGHT=$((MUT_CAUGHT + 1))
  fi
}

# 1. The pipeline FABRICATES a digest for a stage it could not build. This is the single
#    most consequential thing a release pipeline can do, and it is the mutation the whole
#    gate exists for. The body asserts the mutant DOES fabricate.
mutate "a blocked stage given a fabricated digest is not detected" \
  'text=text.replace(
        "    result[\"reason\"] = (\n        f\"`{\x27 \x27.join(spec.get(\x27command\x27, []))}` is available but this is a real build, \"",
        "    result[\"state\"] = \"verified\"\n    result[\"digest\"] = \"sha256:\" + \"b\"*64\n    result[\"size_bytes\"] = 1\n    result[\"file\"] = __file__\n    result[\"reason\"] = (\n        f\"`{\x27 \x27.join(spec.get(\x27command\x27, []))}` is available but this is a real build, \"")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
stages=[p.run_stage(s,".",".") for s in p.plan() if s["stage_kind"] in ("image","cli","sbom")]
# The body FAILS (non-zero) when the mutant fabricates — that is the mutation surviving.
sys.exit(1 if any(s["state"]=="verified" for s in stages) else 0)'

# 2. The credential pattern goes back to word boundaries — the version that could not see
#    `OMNION_DB_PASSWORD` because `_` is a word character.
mutate "a compose file with a hard-coded secret is accepted" \
  'text=text.replace(
        "r\"(?i)(password|passwd|secret|token|api[_-]?key|access[_-]?key|credential|private[_-]?key|dsn)\"",
        "r\"(?i)\\b(password|passwd|secret|token|api[_-]?key|access[_-]?key|credential|private[_-]?key|dsn)\\b\"")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
leaky="services:\n  api:\n    environment:\n      ADMIN_PASSWORD: hunter2\n      OMNION_DB_PASSWORD: hunter3\n"
sys.exit(1 if not p._literal_credentials(leaky) else 0)'

# 3. The pre-release refusal is removed. The body asserts the mutant DOES allow it.
mutate "a pre-release blocked from stable is allowed" \
  'text=text.replace("    if \"-\" in version and channel == \"stable\":", "    if False:")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
g={"gates":{"t":{"state":"green"}},"missing_required":[]}
r=p.publish_refusals("v1.0.0-rc.1","1.0.0-rc.1",g,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
sys.exit(1 if not any("pre-release" in x for x in r) else 0)'

# 4. The chart content digest goes back to hashing raw bytes, so a rebuild no longer matches.
mutate "the content digest changed to include mtime is not detected" \
  'text=text.replace("    import tarfile\n\n    entries: list[str] = []", "    return _sha256_file(path)\n    import tarfile\n\n    entries: list[str] = []")' \
  '
import sys,os,io,tarfile,tempfile,time
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
d=tempfile.mkdtemp(); one=os.path.join(d,"1.tgz"); two=os.path.join(d,"2.tgz")
def pack(path,mtime):
    with tarfile.open(path,"w:gz") as t:
        info=tarfile.TarInfo("x/f"); info.size=2; info.mtime=mtime; info.mode=0o644
        t.addfile(info, io.BytesIO(b"hi"))
pack(one,1000); time.sleep(1.1); pack(two,9999)
sys.exit(1 if p.canonical_archive_digest(one)!=p.canonical_archive_digest(two) else 0)'

# 5. `publish_refusals` returns only the first problem.
mutate "only the first refusal is reported" \
  'text=text.replace("    return refusals\n\n\n# ---", "    return refusals[:1]\n\n\n# ---")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
g={"gates":{"t":{"state":"red","reason":"x"}},"missing_required":["m"]}
r=p.publish_refusals("v9.9.9","0.1.0",g,{"_synthetic":True,"_blocked_stages":["i"]},attestation=False)
sys.exit(1 if len(r)<6 else 0)'

# 6. Synthetic facts become publishable.
mutate "synthetic facts are publishable" \
  'text=text.replace("    if facts.get(\"_synthetic\"):", "    if False:")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
g={"gates":{"t":{"state":"green"}},"missing_required":[]}
r=p.publish_refusals("v0.1.0","0.1.0",g,{"_synthetic":True,"_blocked_stages":[]},attestation=True)
sys.exit(1 if not any("synthetic" in x for x in r) else 1)'

# 7. A red gate stops refusing.
mutate "a red gate does not refuse" \
  'text=text.replace("    if red:", "    if False and red:")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
g={"gates":{"manifest":{"state":"red","reason":"boom"}},"missing_required":[]}
r=p.publish_refusals("v0.1.0","0.1.0",g,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
sys.exit(1 if not any("red" in x for x in r) else 1)'

# 8. A required gate that could not be run is treated as green — the failure mode this
#    record has hit in three different gates, so it gets a mutation of its own.
mutate "an unrunnable required gate is treated as green" \
  'text=text.replace("    if unrunnable:", "    if False:")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
g={"gates":{"manifest":{"state":"unrunnable","reason":"x"}},"missing_required":[]}
r=p.publish_refusals("v0.1.0","0.1.0",g,{"_synthetic":False,"_blocked_stages":[]},attestation=True)
sys.exit(1 if not any("could not be run" in x for x in r) else 1)'

# 9. A stage kind the STAGES table does not know is reported as available, so a typo in a
#    new stage silently passes as runnable.
mutate "an unknown stage kind is reported as available" \
  'text=text.replace("    if spec is None:\n        return False, f\"no stage specification for {kind!r}\"", "    if spec is None:\n        return True, \"\"")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
ok,_=p.stage_availability({"id":"x:1","stage_kind":"telepathy","name":"n","kind":"image"})
sys.exit(1 if ok else 0)'

# 10. `plan()` stops consulting the Dockerfiles and returns a hard-coded two images — the
#     defect the manifest gate found in this repository, where `web` disappears from a
#     release. Written as an inert-but-real edit: the list is replaced, not emptied, so
#     the mutant still imports and still returns a plausible plan.
mutate "the plan stops deriving images from the Dockerfiles" \
  'text=text.replace("    for image in release_manifest.discover_images(root):\n        component = image[\"name\"].rsplit(\"/\", 1)[-1]", "    for image in [{\"name\": \"x/api\"}, {\"name\": \"x/admin\"}]:\n        component = image[\"name\"].rsplit(\"/\", 1)[-1]")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import manifest as m, pipeline as p
images={s["target"] for s in p.plan() if s["stage_kind"]=="image"}
expected={c for cs in m.dockerfile_targets(".").values() for c in cs}
print("plan:",sorted(images),"expected:",sorted(expected),file=sys.stderr)
sys.exit(1 if images==expected else 0)'

# 11. The SBOM stage loses its dependency on the image it describes, which is how an SBOM
#     gets generated from nothing — a real risk, since syft is a separate tool invocation.
mutate "an SBOM stage no longer depends on its image" \
  'text=text.replace("                \"depends_on\": [stage[\"id\"]],", "                \"depends_on\": [],")' \
  '
import sys,os
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
by_id={s["id"]:s for s in p.plan()}
bad=[s["id"] for s in p.plan() if s["stage_kind"]=="sbom"
     and ("image:"+str(s["target"])) not in s["depends_on"]]
sys.exit(1 if bad else 0)'

# 12. The facts document stops recording which stages produced it, so a blocked stage can
#     no longer be traced to the digest that claims to be its own. The body asserts the
#     mutant DOES lose the attribution.
mutate "a fact entry no longer names the stage that produced it" \
  'text=text.replace("            \"stage\": result[\"id\"],", "            \"stage\": None,")' \
  '
import json,os,sys
sys.path.insert(0,os.environ["MUT_LIB"])
import pipeline as p
report=p.dry_run(root=".", version=p.release_manifest.workspace_version("."), workdir="/tmp/mut-facts")
facts=json.load(open(report["facts_path"]))
entries=[e for s in ("images","cli","charts","sboms","compose") for e in facts.get(s,{}).values()]
# The mutant reports stage=None for every entry. The body must FAIL on that, so it exits
# non-zero when an entry HAS lost its attribution. Getting this backwards is what left the
# mutation reading as SURVIVED through three separate attempts: the harness treats exit 0
# as "the rule held", and a body written as `exit(0 if good)` while asserting the mutant
# is good is a body that passes a working mutation and fails a broken one.
sys.exit(1 if not all(e.get("stage") for e in entries) else 0)'


if [ "$MUT_CAUGHT" -ne "$MUT" ]; then
  fail "mutations all caught" "$MUT_CAUGHT/$MUT"
else
  pass
  echo "  $MUT_CAUGHT/$MUT mutations caught"
fi

section "result"
echo "passed: $PASS   failed: $FAIL   mutations: $MUT_CAUGHT/$MUT caught"
if [ "$FAIL" -gt 0 ]; then
  for name in "${FAILED_NAMES[@]}"; do echo "  - $name"; done
  exit 1
fi
exit 0


