"use client";

/**
 * `/settings/iam` — the IAM overview (REQ-006, slice 2).
 *
 * What exists in the organization (accounts, roles, groups, machine identities, live bindings),
 * which temporary grants run out within a week, and the last privileged actions recorded in the
 * audit trail. Every number links to the screen that owns it.
 */
import { useCallback, useEffect, useState } from "react";

import { Bot, FileClock, RefreshCw, ShieldCheck, TimerReset, UserCog, UsersRound } from "lucide-react";
import Link from "next/link";

import { useSession } from "@/lib/session";
import { ApiError, fetchIamOverview, fetchOrganizations, type IamOverview } from "@/lib/api";
import type { Organization } from "@/lib/types";

/** `/settings/iam`. */
export function IamOverviewView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [overview, setOverview] = useState<IamOverview | null>(null);
  const [status, setStatus] = useState<"loading" | "ready" | "error">("loading");
  const [loadError, setLoadError] = useState<{ code: string; message: string } | null>(null);

  const platformAccount = user ? user.organization_id === null : false;
  const activeOrg = platformAccount ? selectedOrg : (user?.organization_id ?? null);

  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) return;
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg(list[0]?.id ?? null);
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations]);

  const load = useCallback(async () => {
    if (!user) return;
    if (platformAccount && !selectedOrg) return;
    setStatus("loading");
    setLoadError(null);
    try {
      setOverview(await fetchIamOverview(platformAccount ? selectedOrg : null));
      setStatus("ready");
    } catch (cause) {
      setStatus("error");
      setLoadError(
        cause instanceof ApiError
          ? { code: cause.code, message: cause.message }
          : { code: "unknown_error", message: "The overview could not be read." },
      );
    }
  }, [user, platformAccount, selectedOrg]);

  useEffect(() => {
    void load();
  }, [load]);

  if (status === "error") {
    return (
      <div className="flex flex-col items-center gap-2 rounded-xl border border-line bg-surface px-6 py-10 text-center">
        <ShieldCheck className="size-4 text-caution" aria-hidden />
        <p className="text-[13.5px] font-medium">The overview is unavailable</p>
        <p className="max-w-md text-[12.5px] text-muted">
          {loadError?.message} <span className="font-mono text-[11.5px]">({loadError?.code})</span>
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-1 flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] font-medium transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  if (!overview) {
    return (
      <div className="rounded-xl border border-line bg-surface p-4" aria-live="polite">
        <span className="sr-only">Reading the overview…</span>
        {[0, 1, 2].map((row) => (
          <div key={row} className="mb-2 h-16 animate-pulse rounded-lg bg-quiet-soft" />
        ))}
      </div>
    );
  }

  const cards = [
    { label: "Accounts", value: overview.counts.users, href: "/settings/iam/users", icon: UserCog },
    { label: "Roles", value: overview.counts.roles, href: "/settings/iam/roles", icon: ShieldCheck },
    { label: "Groups", value: overview.counts.groups, href: "/settings/iam/groups", icon: UsersRound },
    {
      label: "Service accounts",
      value: overview.counts.service_accounts,
      href: "/settings/iam/service-accounts",
      icon: Bot,
    },
    {
      label: "Live bindings",
      value: overview.counts.live_bindings,
      href: "/settings/iam/users",
      icon: TimerReset,
    },
    {
      label: "Expiring in 7 days",
      value: overview.counts.expiring_soon,
      href: "/settings/iam/users",
      icon: TimerReset,
    },
  ];

  return (
    <div className="flex flex-col gap-4">
      {platformAccount && organizations && organizations.length > 0 ? (
        <label className="flex items-center gap-2 text-[12.5px]">
          <span className="text-muted">Organization</span>
          <select
            value={selectedOrg ?? ""}
            data-iam-overview-organization
            onChange={(event) => setSelectedOrg(event.target.value)}
            className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            {organizations.map((organization) => (
              <option key={organization.id} value={organization.id}>
                {organization.name}
              </option>
            ))}
          </select>
        </label>
      ) : null}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {cards.map((card) => {
          const Icon = card.icon;
          return (
            <Link
              key={card.label}
              href={card.href}
              data-iam-overview-card={card.label}
              className="flex items-center justify-between rounded-xl border border-line bg-surface px-4 py-3 transition hover:border-accent/40 hover:bg-accent-soft/40"
            >
              <span className="flex flex-col">
                <span className="text-[12px] text-muted">{card.label}</span>
                <span className="text-[22px] font-semibold tabular-nums">{card.value}</span>
              </span>
              <Icon className="size-4 text-muted" aria-hidden />
            </Link>
          );
        })}
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-4" aria-label="Expiring bindings">
          <p className="flex items-center gap-1.5 text-[13px] font-semibold">
            <TimerReset className="size-3.5 text-muted" aria-hidden />
            Running out within seven days
          </p>
          {overview.expiring.length === 0 ? (
            <p className="text-[12.5px] text-muted">
              No temporary grant is close to its expiry. Temporary roles stop counting on their own;
              the row stays for the history.
            </p>
          ) : (
            <ul className="flex flex-col" data-iam-overview-expiring>
              {overview.expiring.map((entry) => (
                <li
                  key={entry.binding_id}
                  className="flex items-center justify-between gap-2 border-b border-line py-1.5 text-[12.5px] last:border-b-0"
                >
                  <span>
                    {entry.role_name ?? entry.role_key ?? entry.role_id.slice(0, 8)}
                    <span className="ml-1.5 font-mono text-[11px] text-muted">{entry.scope}</span>
                  </span>
                  <span className="text-muted">
                    {entry.expires_at ? entry.expires_at.slice(0, 16).replace("T", " ") : "—"}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </section>

        <section className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-4" aria-label="Recent privileged actions">
          <p className="flex items-center gap-1.5 text-[13px] font-semibold">
            <FileClock className="size-3.5 text-muted" aria-hidden />
            Recent privileged actions
          </p>
          {overview.recent.length === 0 ? (
            <p className="text-[12.5px] text-muted">The audit trail is empty for this organization.</p>
          ) : (
            <ul className="flex flex-col" data-iam-overview-recent>
              {overview.recent.map((entry) => (
                <li
                  key={entry.id}
                  className="flex items-center justify-between gap-2 border-b border-line py-1.5 text-[12.5px] last:border-b-0"
                >
                  <span className="font-mono text-[11.5px]">{entry.action}</span>
                  <span className="text-muted">
                    {entry.target_type ? `${entry.target_type} ` : ""}
                    {entry.created_at.slice(0, 16).replace("T", " ")}
                  </span>
                </li>
              ))}
            </ul>
          )}
          <Link
            href="/settings/iam/roles"
            className="mt-1 self-start text-[12px] font-medium text-accent-strong hover:underline"
          >
            Roles & access →
          </Link>
        </section>
      </div>

      <p className="text-[11.5px] text-muted">
        Sign-in attempts, lockouts, sessions and devices get their own screens with the security
        slice; the counters here only report what already exists.
      </p>
    </div>
  );
}
