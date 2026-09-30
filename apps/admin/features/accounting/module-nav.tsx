"use client";

/**
 * The accounting module's own shelf (REQ-054).
 *
 * The main nav links straight to `/accounting/journal`, because the journal is what somebody
 * opening "Accounting" wants — a module menu that started on a blank overview would spend their
 * first click on navigation. The rest of the module still has to be one click away, though, and a
 * chart of accounts that can only be reached by typing its URL is a screen that does not exist as
 * far as anybody working the module is concerned.
 *
 * **Derived from the pathname, not from a prop.** A subnav that takes "which tab am I on" as an
 * input has to be told by every screen that renders it, and the first one that forgets is a tab
 * that never highlights. Reading the route is the one thing that cannot drift.
 *
 * Slice 1 ships three of the spec's eight screens. The other five are **absent, not stubbed**:
 * a tab that links to a screen slice 2 has not built yet is a dead button, and a dead button is
 * the one thing the definition of done forbids outright. The tab appears the tick its route does.
 */
import Link from "next/link";
import { usePathname } from "next/navigation";
import { BookOpen, Percent, ReceiptText, ScrollText } from "lucide-react";

const LINKS: { href: string; label: string; icon: typeof BookOpen }[] = [
  { href: "/accounting/journal", label: "Journal", icon: ScrollText },
  { href: "/accounting/accounts", label: "Chart of accounts", icon: BookOpen },
  { href: "/accounting/tax-rates", label: "Tax rates", icon: Percent },
  { href: "/accounting/invoices", label: "Invoices", icon: ReceiptText },
];

export function AccountingModuleNav() {
  const pathname = usePathname();
  return (
    <nav
      aria-label="Accounting"
      data-qa-accounting-module-nav
      className="flex flex-wrap items-center gap-1 border-b border-border pb-2"
    >
      {LINKS.map((link) => {
        const here = pathname === link.href || pathname?.startsWith(`${link.href}/`);
        const Icon = link.icon;
        return (
          <Link
            key={link.href}
            href={link.href}
            data-qa-accounting-module-link={link.href.split("/").pop()}
            aria-current={here ? "page" : undefined}
            className={`inline-flex h-8 items-center gap-1.5 rounded-md px-2.5 text-sm ${
              here
                ? "bg-muted font-medium text-foreground"
                : "text-muted-foreground hover:text-foreground"
            }`}
          >
            <Icon className="h-4 w-4" aria-hidden />
            {link.label}
          </Link>
        );
      })}
    </nav>
  );
}
