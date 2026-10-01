"use client";

/**
 * `/security/ip-access` — the allow and deny lists, and the tester (REQ-012, slice 4).
 *
 * The one screen in the security centre whose mistakes are visible from outside the panel: a
 * deny rule that is wrong does not look wrong here, it looks right, and somebody on the other
 * side of it cannot sign in. Every part of this screen is built around that asymmetry.
 *
 * 1. **A rule has to say why it exists.** The server refuses a blank note and this form requires
 *    one, because an unexplained access rule is one an operator removes without reading during a
 *    panic, or leaves in place without understanding. The note is the first column, not a detail.
 * 2. **The self-lockout warning is loud, and the rule is still saved.** `blocks_you` comes back
 *    from the server because only the server knows the address this request came from. The screen
 *    shows it as a warning beside the list — not as a refusal — for the reason the API comment
 *    gives: locking yourself out of one route is a legitimate move, and blocking it would only
 *    teach the operator which input avoids the check.
 * 3. **Deny wins over allow, and the screen says so where the two can meet.** The precedence is
 *    not folklore here: the tester resolves any address against the stored rules and returns the
 *    rule that decided, so an operator who wonders "why is my allowed office blocked" can type
 *    their address and be told which of their own rules fired.
 * 4. **Expired rules stay on screen, greyed, with their date.** They are inert — an expired deny
 *    does not deny — and hiding them would make an incident block that timed out look like it
 *    never existed. The tester names an expired match for the same reason: its *absence* is the
 *    explanation for a deny that stopped applying.
 * 5. **The empty state is a fact.** "No rules" means the access list is not in force, which is
 *    a different thing from "the list is empty because everything matched" and is stated in those
 *    words rather than rendered as an empty table.
 *
 * Keyboard: the tester is a real form (Enter submits), the delete buttons are reachable in table
 * order, and each rule row carries its own accessible name. Mobile: the two tables become two
 * card stacks rather than one horizontally scrolling page — the QA plan asks for exactly this,
 * because a CIDR table scrolled sideways is unreadable on a phone.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  Loader2,
  MapPin,
  Plus,
  Search,
  ShieldOff,
  Trash2,
} from "lucide-react";

import {
  createIpRule,
  deleteIpRule,
  fetchIpRules,
  testIpAddress,
  type ApiError,
} from "@/lib/api";
import { SecurityTabs } from "@/features/security/security-tabs";
import type { IpRule, IpRulesPage, IpTestResult } from "@/lib/types";

/** The create form's state. `expiry` is a date input's value: `yyyy-mm-dd` or empty. */
type Draft = { kind: "allow" | "deny"; cidr: string; note: string; expiry: string };

const EMPTY_DRAFT: Draft = { kind: "deny", cidr: "", note: "", expiry: "" };

/** A date input's `yyyy-mm-dd` value as an RFC 3339 instant at the end of that day. */
function expiryToRfc3339(value: string): string | null {
  if (!value) return null;
  const parsed = new Date(`${value}T23:59:59Z`);
  if (Number.isNaN(parsed.getTime())) return null;
  return parsed.toISOString();
}

function formatWhen(value: string | null): string {
  if (!value) return "never";
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return value;
  return parsed.toLocaleString();
}

export function IpAccessScreen() {
  const [page, setPage] = useState<IpRulesPage | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [warning, setWarning] = useState<string | null>(null);
  const [probe, setProbe] = useState("");
  const [probeResult, setProbeResult] = useState<IpTestResult | null>(null);
  const [probeError, setProbeError] = useState<string | null>(null);
  const [probing, setProbing] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoadError(null);
    try {
      setPage(await fetchIpRules());
    } catch (error) {
      setLoadError((error as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const rules = page?.rules ?? [];
  const allow = useMemo(() => rules.filter((rule) => rule.kind === "allow"), [rules]);
  const deny = useMemo(() => rules.filter((rule) => rule.kind === "deny"), [rules]);
  const liveCount = useMemo(() => rules.filter((rule) => !rule.expired).length, [rules]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    setFormError(null);
    setWarning(null);
    setSaving(true);
    try {
      const created = await createIpRule({
        kind: draft.kind,
        cidr: draft.cidr,
        note: draft.note,
        expires_at: expiryToRfc3339(draft.expiry),
      });
      setDraft(EMPTY_DRAFT);
      // The warning is kept *after* the list is reloaded, because the list is what it is about.
      setWarning(created.warning);
      await load();
    } catch (error) {
      setFormError((error as ApiError).message);
    } finally {
      setSaving(false);
    }
  }

  async function remove(rule: IpRule) {
    setRemoving(rule.id);
    setWarning(null);
    try {
      await deleteIpRule(rule.id);
      await load();
    } catch (error) {
      setFormError((error as ApiError).message);
    } finally {
      setRemoving(null);
    }
  }

  async function runProbe(event: React.FormEvent) {
    event.preventDefault();
    setProbeError(null);
    setProbeResult(null);
    setProbing(true);
    try {
      setProbeResult(await testIpAddress(probe));
    } catch (error) {
      setProbeError((error as ApiError).message);
    } finally {
      setProbing(false);
    }
  }

  return (
    <div className="space-y-5" data-ip-access>
      <SecurityTabs current="ip-access" />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold text-ink">IP access rules</h1>
          <p className="mt-1 max-w-2xl text-[13px] text-muted">
            Networks that may reach this API, and networks that may not. When an address matches
            both lists, the deny rule wins.
          </p>
        </div>
        <p className="text-[12.5px] text-muted" data-ip-access-counts>
          {loading
            ? "Reading the lists…"
            : `${page?.deny_count ?? 0} denied · ${page?.allow_count ?? 0} allowed`}
        </p>
      </header>

      {/* The self-lockout warning. Stated once, above the list it is about. */}
      {warning ? (
        <p
          role="alert"
          data-ip-access-warning
          className="flex items-start gap-2 rounded-lg border border-warn/40 bg-warn-soft px-3 py-2 text-[12.5px] text-warn-strong"
        >
          <AlertTriangle className="mt-0.5 size-4 shrink-0" aria-hidden />
          <span>{warning}</span>
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        {/* -- create --------------------------------------------------------------------- */}
        <form
          onSubmit={submit}
          className="space-y-3 rounded-lg border border-line bg-surface p-4"
          data-ip-access-form
        >
          <h2 className="text-[13px] font-semibold text-ink">Add a rule</h2>

          <fieldset className="space-y-1">
            <legend className="text-[12px] font-medium text-ink">List</legend>
            <div className="flex gap-2">
              {(["deny", "allow"] as const).map((kind) => (
                <label
                  key={kind}
                  className={`flex cursor-pointer items-center gap-2 rounded-md border px-2.5 py-1.5 text-[12.5px] ${
                    draft.kind === kind
                      ? "border-accent bg-accent-soft text-accent-strong"
                      : "border-line text-muted"
                  }`}
                >
                  <input
                    type="radio"
                    name="ip-rule-kind"
                    value={kind}
                    checked={draft.kind === kind}
                    onChange={() => setDraft((current) => ({ ...current, kind }))}
                    className="sr-only"
                  />
                  {kind === "deny" ? "Deny list" : "Allow list"}
                </label>
              ))}
            </div>
          </fieldset>

          <div className="space-y-1">
            <label htmlFor="ip-rule-cidr" className="block text-[12px] font-medium text-ink">
              Network
            </label>
            <input
              id="ip-rule-cidr"
              value={draft.cidr}
              onChange={(event) =>
                setDraft((current) => ({ ...current, cidr: event.target.value }))
              }
              placeholder="203.0.113.0/24, or one address"
              data-ip-access-cidr
              aria-describedby="ip-rule-cidr-help"
              className="w-full rounded-md border border-line bg-quiet px-2.5 py-1.5 text-[13px] text-ink placeholder:text-muted-soft"
            />
            <p id="ip-rule-cidr-help" className="text-[11.5px] text-muted">
              A single address is fine — it becomes a /32. The prefix must cover the whole network,
              so <code className="text-[11px]">203.0.113.0/24</code> rather than{" "}
              <code className="text-[11px]">203.0.113.7/24</code>.
            </p>
          </div>

          <div className="space-y-1">
            <label htmlFor="ip-rule-note" className="block text-[12px] font-medium text-ink">
              Why this rule exists
            </label>
            <input
              id="ip-rule-note"
              value={draft.note}
              onChange={(event) =>
                setDraft((current) => ({ ...current, note: event.target.value }))
              }
              placeholder="Contractor's VPN, retired March"
              data-ip-access-note
              className="w-full rounded-md border border-line bg-quiet px-2.5 py-1.5 text-[13px] text-ink placeholder:text-muted-soft"
            />
          </div>

          <div className="space-y-1">
            <label htmlFor="ip-rule-expiry" className="block text-[12px] font-medium text-ink">
              Expires (optional)
            </label>
            <input
              id="ip-rule-expiry"
              type="date"
              value={draft.expiry}
              onChange={(event) =>
                setDraft((current) => ({ ...current, expiry: event.target.value }))
              }
              data-ip-access-expiry
              className="w-full rounded-md border border-line bg-quiet px-2.5 py-1.5 text-[13px] text-ink"
            />
            <p className="text-[11.5px] text-muted">
              A temporary block stops applying on its own, which is what an incident response
              wants. Leave it empty for a rule that never expires.
            </p>
          </div>

          {formError ? (
            <p role="alert" data-ip-access-error className="text-[12.5px] text-danger">
              {formError}
            </p>
          ) : null}

          <button
            type="submit"
            disabled={saving}
            data-ip-access-submit
            className="inline-flex items-center gap-1.5 rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-ink disabled:opacity-60"
          >
            {saving ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <Plus className="size-3.5" aria-hidden />
            )}
            Add rule
          </button>
        </form>

        {/* -- tester --------------------------------------------------------------------- */}
        <form
          onSubmit={runProbe}
          className="space-y-3 rounded-lg border border-line bg-surface p-4"
          data-ip-access-tester
        >
          <h2 className="text-[13px] font-semibold text-ink">Test an address</h2>
          <p className="text-[12px] text-muted">
            One address, not a network. The verdict comes from the same evaluator that decides
            real requests, and it names the rule that decided.
          </p>
          <div className="flex gap-2">
            <label htmlFor="ip-probe" className="sr-only">
              Address to test
            </label>
            <input
              id="ip-probe"
              value={probe}
              onChange={(event) => setProbe(event.target.value)}
              placeholder="203.0.113.7"
              data-ip-access-probe-input
              className="w-full rounded-md border border-line bg-quiet px-2.5 py-1.5 text-[13px] text-ink placeholder:text-muted-soft"
            />
            <button
              type="submit"
              disabled={probing || probe.trim() === ""}
              data-ip-access-probe
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] font-medium text-ink disabled:opacity-60"
            >
              {probing ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <Search className="size-3.5" aria-hidden />
              )}
              Test
            </button>
          </div>

          {probeError ? (
            <p role="alert" data-ip-access-probe-error className="text-[12.5px] text-danger">
              {probeError}
            </p>
          ) : null}

          {probeResult ? (
            <div
              data-ip-access-probe-result
              data-blocked={probeResult.blocked ? "true" : "false"}
              className={`rounded-md border px-3 py-2 text-[12.5px] ${
                probeResult.blocked
                  ? "border-danger/40 bg-danger-soft text-danger"
                  : "border-ok/40 bg-ok-soft text-ok-strong"
              }`}
            >
              <p className="flex items-center gap-1.5 font-medium">
                {probeResult.blocked ? (
                  <ShieldOff className="size-3.5" aria-hidden />
                ) : (
                  <CheckCircle2 className="size-3.5" aria-hidden />
                )}
                {probeResult.blocked ? "Blocked" : "Allowed"}
              </p>
              <p className="mt-1">{probeResult.reason}</p>
              {probeResult.matched_rule ? (
                <p className="mt-1 font-mono text-[11.5px]">
                  matched {probeResult.matched_rule.cidr} — {probeResult.matched_rule.note}
                </p>
              ) : null}
              {probeResult.expired_rule ? (
                <p
                  data-ip-access-probe-expired
                  className="mt-1 text-[11.5px] text-muted"
                >
                  {probeResult.expired_rule.cidr} would have matched, but it expired on{" "}
                  {formatWhen(probeResult.expired_rule.expires_at)} — an expired rule does not
                  apply.
                </p>
              ) : null}
            </div>
          ) : null}
        </form>
      </div>

      {loading ? (
        <p className="flex items-center gap-2 text-[13px] text-muted" data-ip-access-loading>
          <Loader2 className="size-4 animate-spin" aria-hidden />
          Loading the access lists…
        </p>
      ) : loadError ? (
        <div
          role="alert"
          className="flex items-center justify-between gap-3 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-danger"
        >
          <span>{loadError}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded-md border border-danger/40 px-2 py-1 font-medium"
          >
            Retry
          </button>
        </div>
      ) : rules.length === 0 ? (
        <div
          data-ip-access-empty
          className="rounded-lg border border-dashed border-line px-4 py-8 text-center"
        >
          <MapPin className="mx-auto size-5 text-muted-soft" aria-hidden />
          <p className="mt-2 text-[13px] font-medium text-ink">No IP rules are in force</p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            Every address can reach the API. That is the default on a new installation — add a deny
            rule above to take an address range out of service.
          </p>
        </div>
      ) : (
        <div className="space-y-4">
          <RuleTable
            title="Denied"
            caption={`${deny.length} rule${deny.length === 1 ? "" : "s"}`}
            rules={deny}
            onRemove={remove}
            removing={removing}
            liveCount={liveCount}
          />
          <RuleTable
            title="Allowed"
            caption={`${allow.length} rule${allow.length === 1 ? "" : "s"}`}
            rules={allow}
            onRemove={remove}
            removing={removing}
            liveCount={liveCount}
          />
        </div>
      )}
    </div>
  );
}

/**
 * One list's table, and its card list at `sm` and below.
 *
 * `liveCount` is passed rather than recomputed so the caption can say "3 rules, 1 expired"
 * without each table counting rows the other one already counted — two independent counts of
 * the same array is the kind of drift this section has otherwise been bitten by.
 */
function RuleTable({
  title,
  caption,
  rules,
  onRemove,
  removing,
  liveCount,
}: {
  title: string;
  caption: string;
  rules: IpRule[];
  onRemove: (rule: IpRule) => void | Promise<void>;
  removing: string | null;
  liveCount: number;
}) {
  if (rules.length === 0) {
    return (
      <section className="rounded-lg border border-line bg-surface p-4" data-ip-rule-list={title}>
        <h2 className="text-[13px] font-semibold text-ink">{title}</h2>
        <p className="mt-1 text-[12.5px] text-muted">
          No {title.toLowerCase()} rules. Nothing is decided by this list.
        </p>
      </section>
    );
  }

  const expiredHere = rules.length - rules.filter((rule) => !rule.expired).length;

  return (
    <section className="rounded-lg border border-line bg-surface" data-ip-rule-list={title}>
      <header className="flex flex-wrap items-baseline justify-between gap-2 border-b border-line px-4 py-3">
        <h2 className="text-[13px] font-semibold text-ink">{title}</h2>
        <p className="text-[12px] text-muted">
          {caption}
          {expiredHere > 0 ? ` · ${expiredHere} expired` : ""}
          {liveCount === 0 ? " · none in force" : ""}
        </p>
      </header>

      {/* Table from `sm` up. */}
      <table className="hidden w-full text-left text-[12.5px] sm:block">
        <caption className="sr-only">{title} list</caption>
        <thead>
          <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
            <th scope="col" className="px-4 py-2 font-medium">Network</th>
            <th scope="col" className="px-4 py-2 font-medium">Why</th>
            <th scope="col" className="px-4 py-2 font-medium">Added</th>
            <th scope="col" className="px-4 py-2 font-medium">Expires</th>
            <th scope="col" className="px-4 py-2 font-medium">
              <span className="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {rules.map((rule) => (
            <tr
              key={rule.id}
              data-ip-rule={rule.id}
              data-expired={rule.expired ? "true" : "false"}
              className={`border-b border-line last:border-0 ${
                rule.expired ? "text-muted-soft" : "text-ink"
              }`}
            >
              <td className="px-4 py-2 font-mono text-[12px]">
                {rule.cidr}
                {rule.expired ? (
                  <span className="ml-2 rounded bg-quiet px-1 py-0.5 text-[10.5px] font-sans uppercase tracking-wide">
                    expired
                  </span>
                ) : null}
              </td>
              <td className="px-4 py-2">{rule.note}</td>
              <td className="px-4 py-2 text-muted">{formatWhen(rule.created_at)}</td>
              <td className="px-4 py-2 text-muted">{formatWhen(rule.expires_at)}</td>
              <td className="px-4 py-2 text-right">
                <button
                  type="button"
                  onClick={() => void onRemove(rule)}
                  disabled={removing === rule.id}
                  data-ip-rule-remove={rule.id}
                  aria-label={`Remove the ${rule.kind} rule for ${rule.cidr}`}
                  className="rounded-md border border-line p-1.5 text-muted hover:text-danger disabled:opacity-50"
                >
                  {removing === rule.id ? (
                    <Loader2 className="size-3.5 animate-spin" aria-hidden />
                  ) : (
                    <Trash2 className="size-3.5" aria-hidden />
                  )}
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {/* Cards at `sm` and below — a CIDR table scrolled sideways is unreadable on a phone. */}
      <ul className="divide-y divide-line sm:hidden">
        {rules.map((rule) => (
          <li
            key={rule.id}
            data-ip-rule-card={rule.id}
            className={`space-y-1 px-4 py-3 ${rule.expired ? "text-muted-soft" : "text-ink"}`}
          >
            <div className="flex items-center justify-between gap-2">
              <span className="font-mono text-[12.5px]">{rule.cidr}</span>
              <button
                type="button"
                onClick={() => void onRemove(rule)}
                disabled={removing === rule.id}
                aria-label={`Remove the ${rule.kind} rule for ${rule.cidr}`}
                className="rounded-md border border-line p-1.5 text-muted"
              >
                <Trash2 className="size-3.5" aria-hidden />
              </button>
            </div>
            <p className="text-[12.5px]">{rule.note}</p>
            <p className="text-[11.5px] text-muted">
              Added {formatWhen(rule.created_at)} · expires {formatWhen(rule.expires_at)}
            </p>
          </li>
        ))}
      </ul>
    </section>
  );
}