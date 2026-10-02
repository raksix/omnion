"use client";

/**
 * `/ai/local/guide` — "Run AI locally" (docs/requests/REQ-106, slice 4).
 *
 * The operator-facing manual, and the reason it is **not** a markdown file is the point of the
 * screen: a static page cannot know whether the claims on it are true on *this* installation, and
 * the failure this page exists to prevent is exactly that — an operator who follows the
 * verification steps, gets a green result from the doctor, and concludes the installation is
 * offline-capable while a collection is still pinned to a remote embedding model.
 *
 * So the guidance is **rendered from the same three endpoints the operator would otherwise have to
 * open by hand**: `fetchLocalEndpoints`, `fetchAirgap` and `fetchDoctor`. Every step below states
 * what it is telling you, and where the claim comes from:
 *
 * 1. **The steps are numbered in the order they actually work.** Register → verify → pull →
 *    default → doctor → air gap. Each step names the screen that performs it, so the page is a
 *    map rather than a wall of prose, and every link is a real route.
 *
 * 2. **The "what stops working" list is computed, not asserted.** It is built from
 *    `would_block` — the provider list the *call path* asks before it refuses — so this page
 *    cannot claim a provider is safe while the check would block it. A hand-written list would be a
 *    second copy of the locality rule, which is the failure mode that matters most on this screen.
 *
 * 3. **The readiness line is the doctor's verdict, or an explicit "never run".** Not a computed
 *    re-derivation of what the checks "should" say: the doctor is the thing that dials endpoints,
 *    and a page that guessed would be green on a box with no local server at all.
 *
 * The server list is deliberately short and deliberately honest about the protocol: every supported
 * server speaks `openai_compatible`, which is the only protocol this build accepts for a local
 * endpoint, so naming a client that needs `/api/chat` would name something that cannot be
 * registered.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import {
  CircleCheck,
  CircleDashed,
  Cpu,
  FileText,
  Stethoscope,
  TriangleAlert,
} from "lucide-react";

import { ApiError } from "@/lib/api";
import { fetchAirgap, type AirgapOverview } from "@/lib/airgap-api";
import { fetchDoctor, type DoctorList } from "@/lib/local-api";
import { fetchLocalEndpoints, type LocalEndpointList } from "@/lib/local-api";

/** The servers this build can register, and nothing else. */
const SUPPORTED_SERVERS = [
  {
    name: "Ollama",
    baseUrl: "http://127.0.0.1:11434/v1",
    note: "OpenAI-compatible endpoint shipped by Ollama itself. The default choice for a single GPU box.",
  },
  {
    name: "vLLM",
    baseUrl: "http://127.0.0.1:8000/v1",
    note: "Serves one model per process. Start it with an explicit --model so the key you register below is the one it answers for.",
  },
  {
    name: "LM Studio / llama.cpp server",
    baseUrl: "http://127.0.0.1:1234/v1",
    note: "The llama.cpp OpenAI-compatible server, which is also what several desktop clients expose on the same shape.",
  },
  {
    name: "Any OpenAI-compatible server",
    baseUrl: "http://<private-host>:<port>/v1",
    note: "The protocol is the contract, not the product. The host must still pass the locality check below.",
  },
];

/** Why a failed read reads as a sentence rather than a code. */
function reason(error: unknown): string {
  if (error instanceof ApiError) {
    return error.code === "forbidden"
      ? `This account may not read the local-AI configuration (${error.message}).`
      : error.message;
  }
  return error instanceof Error ? error.message : String(error);
}

/** The verdict word → tone. `never run` is deliberately not one of the three. */
function verdictTone(doctor: DoctorList | null): { label: string; tone: string; note: string } {
  // `latest` is the run, `previous` is its history — and `never_run` is the server's own word for
  // "there is no data". Reading `latest` rather than the flag is the point: the flag is a cached
  // claim about the same fact, and a page that trusts it re-derives the risk this whole feature
  // exists to remove.
  const latest = doctor?.latest ?? null;
  if (!latest) {
    return {
      label: "Never verified",
      tone: "text-muted",
      note: "Run the doctor before you read anything else on this page as fact.",
    };
  }
  switch (latest.status) {
    case "passed":
      return {
        label: "Ready for air-gapped operation",
        tone: "text-positive",
        note: `Last checked ${latest.finished_at ?? latest.started_at}.`,
      };
    case "warned":
      return {
        label: "Usable, with warnings",
        tone: "text-caution",
        note: "Some checks are warnings rather than failures. Open the doctor to see which.",
      };
    default:
      return {
        label: "Not ready",
        tone: "text-danger",
        note: "At least one check failed. The doctor names the check and the fix.",
      };
  }
}

/** One numbered step, rendered the same way for every step. */
function Step(props: {
  index: number;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <li className="border-l-2 border-line pl-4" data-guide-step={props.index}>
      <p className="text-[13px] font-medium">
        <span className="mr-1.5 text-muted">{props.index}.</span>
        {props.title}
      </p>
      <div className="mt-1 text-[13px] leading-relaxed text-muted">{props.children}</div>
    </li>
  );
}

export function LocalGuideView() {
  const [endpoints, setEndpoints] = useState<LocalEndpointList | null>(null);
  const [airgap, setAirgap] = useState<AirgapOverview | null>(null);
  const [doctor, setDoctor] = useState<DoctorList | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    // Three independent reads. `allSettled` rather than `Promise.all`: a doctor that 403s must not
    // hide the endpoint list, and the page is useful with one section refused as long as it says
    // which one — a single blanket failure would send the operator to the wrong screen.
    const [endpointsResult, airgapResult, doctorResult] = await Promise.allSettled([
      fetchLocalEndpoints(),
      fetchAirgap(),
      fetchDoctor(),
    ]);
    const failed = [endpointsResult, airgapResult, doctorResult].filter(
      (entry) => entry.status === "rejected",
    ) as PromiseRejectedResult[];
    if (failed.length === 3) {
      setError(reason(failed[0].reason));
      setLoading(false);
      return;
    }
    if (endpointsResult.status === "fulfilled") setEndpoints(endpointsResult.value);
    if (airgapResult.status === "fulfilled") setAirgap(airgapResult.value);
    if (doctorResult.status === "fulfilled") setDoctor(doctorResult.value);
    setLoading(false);
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const localEndpoints = useMemo(
    () => (endpoints?.endpoints ?? []).filter((endpoint) => endpoint.locality === "local"),
    [endpoints],
  );
  const wouldBlock = airgap?.would_block ?? [];
  const verdict = useMemo(() => verdictTone(doctor), [doctor]);

  if (loading) {
    return (
      <p className="text-[13px] text-muted" role="status">
        Reading this installation's local-AI state…
      </p>
    );
  }

  if (error) {
    return (
      <div className="rounded-lg border border-line bg-panel p-4" role="alert">
        <p className="text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-muted/40"
        >
          Try again
        </button>
      </div>
    );
  }

  return (
    <div className="space-y-5" data-local-guide>
      {/* The readiness line. Computed from the doctor's own verdict, never re-derived here. */}
      <section
        className="rounded-lg border border-line bg-panel p-4"
        aria-labelledby="local-guide-verdict"
      >
        <h2
          id="local-guide-verdict"
          className="flex items-center gap-2 text-[14px] font-medium"
        >
          {verdict.tone === "text-positive" ? (
            <CircleCheck className="h-4 w-4" aria-hidden="true" />
          ) : (
            <TriangleAlert className="h-4 w-4" aria-hidden="true" />
          )}
          <span data-guide-verdict className={verdict.tone}>
            {verdict.label}
          </span>
        </h2>
        <p className="mt-1.5 text-[13px] text-muted" data-guide-verdict-note>
          {verdict.note}
        </p>
        <p className="mt-3 text-[13px] leading-relaxed text-muted">
          This page describes what this build does. It is rendered from your installation's own
          endpoint, air-gap and doctor state, so the numbers below are measured rather than
          promised — and a claim on it that your configuration does not meet shows up as a gap, not
          as a surprise in production.
        </p>
      </section>

      {/* The steps, in the order they work. */}
      <section
        className="rounded-lg border border-line bg-panel p-4"
        aria-labelledby="local-guide-steps"
      >
        <h2
          id="local-guide-steps"
          className="flex items-center gap-2 text-[14px] font-medium"
        >
          <FileText className="h-4 w-4" aria-hidden="true" />
          Running inference locally, step by step
        </h2>

        <ol className="mt-4 space-y-4">
          <Step index={1} title="Register the local server as a provider">
            Add it on the <Link href="/ai/local" className="underline underline-offset-2">Local AI</Link>{" "}
            screen with the base URL including the <code className="font-mono">/v1</code> segment.
            The host is checked, not trusted: loopback, an RFC 1918 / ULA address or a host on the
            internal allow-list pass, and anything else is refused with the host named — because a
            "local" provider pointing at the public internet is a misconfiguration, not a policy
            choice.
          </Step>

          <Step index={2} title="Ask the server what it serves">
            Use <strong>Scan</strong> on the endpoint row. Scanning is a separate, deliberate action
            rather than a side effect of saving a URL: it writes the server's answer into the model
            table, and until it runs the table honestly says "not checked yet" instead of showing a
            green badge nobody measured.
          </Step>

          <Step index={3} title="Pull the models you want resident">
            <strong>Pull</strong> on the model row asks the local server to download the weights and
            reports its progress; the row moves through <code className="font-mono">pulling</code> to{" "}
            <code className="font-mono">available</code>. Omnion does not download models onto its
            own disk — the local server owns its cache, and a pull that fails shows the server's
            own error verbatim so you get its diagnosis rather than a paraphrase.
          </Step>

          <Step index={4} title="Make the local model the default">
            <strong>Set as default</strong> on the model row. Routing falls back to a remote
            provider whenever the local one is missing, slow or failing, so a default alone is not
            isolation — step 6 is.
          </Step>

          <Step index={5} title="Verify with the doctor before you disconnect anything">
            The <Link href="/ai/local/doctor" className="underline underline-offset-2">doctor</Link>{" "}
            runs each check on demand — endpoint reachable, model present, a one-token completion,
            air-gap state — and prints a plain-language cause and a suggested fix for each. Its
            verdict is what the line above quotes; a page that guessed the verdict would be green
            on a box with no local server at all.
          </Step>

          <Step index={6} title="Turn the air gap on, with a reason">
            The <Link href="/ai/settings/airgap" className="underline underline-offset-2">air-gap switch</Link>{" "}
            refuses every call whose resolved provider is not local, before any network call, with a
            reason attached to the switch and an acknowledgement of the providers that will stop
            answering. Enabling it does not require the doctor's verdict, which is deliberate: the
            gap is a policy you assert and the doctor is evidence — so an operator who cannot reach
            the local server can still refuse to send data out.
          </Step>
        </ol>
      </section>

      {/* The supported servers. Protocol is the contract. */}
      <section
        className="rounded-lg border border-line bg-panel"
        aria-labelledby="local-guide-servers"
      >
        <h2
          id="local-guide-servers"
          className="flex items-center gap-2 border-b border-line px-4 py-3 text-[14px] font-medium"
        >
          <Cpu className="h-4 w-4" aria-hidden="true" />
          Supported servers
        </h2>
        <ul className="divide-y divide-line">
          {SUPPORTED_SERVERS.map((server) => (
            <li key={server.name} className="px-4 py-3" data-guide-server={server.name}>
              <p className="text-[13px] font-medium">{server.name}</p>
              <p className="mt-0.5 font-mono text-[12px] text-muted">{server.baseUrl}</p>
              <p className="mt-1 text-[13px] leading-relaxed text-muted">{server.note}</p>
            </li>
          ))}
        </ul>
        <p className="border-t border-line px-4 py-3 text-[12.5px] text-muted">
          Every one of them speaks the OpenAI-compatible protocol, which is the only protocol a
          local endpoint may declare in this build. A client that only exposes its own native API
          cannot be registered — that is a protocol difference, not a configuration mistake.
        </p>
      </section>

      {/* What stops working while the gap is on — computed from the call path's own list. */}
      <section
        className="rounded-lg border border-line bg-panel"
        aria-labelledby="local-guide-blocks"
      >
        <h2
          id="local-guide-blocks"
          className="flex items-center gap-2 border-b border-line px-4 py-3 text-[14px] font-medium"
        >
          <TriangleAlert className="h-4 w-4" aria-hidden="true" />
          What the air gap stops
        </h2>

        {wouldBlock.length === 0 ? (
          <p className="px-4 py-4 text-[13px] text-muted" data-guide-blocks-empty>
            Nothing on this installation would be refused: every configured provider resolves to a
            local host. That is either a fully local setup or an empty one — the endpoint count
            below tells the two apart.
          </p>
        ) : (
          <ul className="divide-y divide-line" data-guide-blocks>
            {wouldBlock.map((provider) => (
              <li
                key={provider.base_url}
                className="flex flex-wrap items-baseline gap-x-3 px-4 py-2.5"
                data-guide-block={provider.base_url}
              >
                <span className="text-[13px] font-medium">{provider.name}</span>
                <span className="font-mono text-[12px] text-muted">{provider.base_url}</span>
              </li>
            ))}
          </ul>
        )}

        <div className="border-t border-line px-4 py-3">
          <p className="text-[13px] leading-relaxed text-muted">
            While the gap is on, these providers are refused with{" "}
            <code className="font-mono">403 ai_airgap_blocked</code>, naming the provider and the
            host, and the attempt is logged with status{" "}
            <code className="font-mono">blocked_airgap</code>. The refusal happens before the
            network call, so no request is ever sent and later found wanting.
          </p>
          <p className="mt-2 text-[13px] leading-relaxed text-muted">
            Remote <em>embeddings</em> are the case people miss: a knowledge collection pinned to a
            remote embedding model keeps its remote shape even when chat is fully local, so index
            and retrieval have to be repointed at a local embedding model before the installation
            is genuinely offline. That work belongs to the AI memory request; this build does not
            yet ship knowledge collections, so there is nothing to repoint here.
          </p>
        </div>
      </section>

      {/* This installation's numbers, so "is any of this true for me" has an answer on the page. */}
      <section
        className="rounded-lg border border-line bg-panel p-4"
        aria-labelledby="local-guide-yours"
      >
        <h2
          id="local-guide-yours"
          className="flex items-center gap-2 text-[14px] font-medium"
        >
          <Stethoscope className="h-4 w-4" aria-hidden="true" />
          On this installation
        </h2>

        <dl className="mt-3 grid gap-3 sm:grid-cols-4">
          <div className="rounded-md border border-line px-3 py-2" data-guide-stat-local>
            <dt className="text-[12px] text-muted">Local endpoints</dt>
            <dd className="text-[18px] font-medium">{localEndpoints.length}</dd>
          </div>
          <div className="rounded-md border border-line px-3 py-2" data-guide-stat-remote>
            <dt className="text-[12px] text-muted">Remote endpoints</dt>
            <dd className="text-[18px] font-medium">{endpoints?.remote_count ?? 0}</dd>
          </div>
          <div className="rounded-md border border-line px-3 py-2" data-guide-stat-blocked>
            <dt className="text-[12px] text-muted">Would be blocked</dt>
            <dd className="text-[18px] font-medium">{wouldBlock.length}</dd>
          </div>
          <div className="rounded-md border border-line px-3 py-2" data-guide-stat-gap>
            <dt className="text-[12px] text-muted">Air gap</dt>
            <dd className="text-[18px] font-medium" data-guide-gap-state>
              {airgap?.state.enabled ? (
                <span className="text-positive">On</span>
              ) : airgap ? (
                <span className="text-muted">Off</span>
              ) : (
                <span className="text-muted">Unknown</span>
              )}
            </dd>
          </div>
        </dl>

        {localEndpoints.length === 0 ? (
          <p
            className="mt-3 flex items-start gap-2 rounded-md border border-line bg-muted/40 px-3 py-2 text-[13px]"
            data-guide-no-local
          >
            <CircleDashed className="mt-0.5 h-4 w-4 shrink-0" aria-hidden="true" />
            <span>
              No local endpoint is registered, so nothing on this page is active for you yet. Start
              at step 1 — and read the doctor as <em>never verified</em>, not as passing.
            </span>
          </p>
        ) : (
          <p className="mt-3 text-[13px] text-muted">
            Local endpoints on file:{" "}
            {localEndpoints.map((endpoint) => endpoint.name).join(", ")}.
          </p>
        )}
      </section>
    </div>
  );
}