"use client";

/**
 * The inventory module's own shelf (REQ-053).
 *
 * The main nav links straight to `/inventory/stock`, because a stock list is what somebody
 * opening "Inventory" wants — a module menu that started on a blank overview would spend their
 * first click on navigation. The rest of the module still has to be one click away, though, and
 * a transfer or an alert that can only be reached by typing its URL is a screen that does not
 * exist as far as anybody working the module is concerned.
 *
 * **Derived from the pathname, not from a prop.** A subnav that takes "which tab am I on" as an
 * input has to be told by every screen that renders it, and the first one that forgets is a tab
 * that never highlights. Reading the route is the one thing that cannot drift.
 *
 * The pieces the spec names — stock, movements, transfers, low stock — are here, and the two
 * slice-2 screens (the adjustment inbox) are here too, because a shelf that lists four of a
 * module's six screens is a shelf that hides two.
 */
import Link from "next/link";
import { usePathname } from "next/navigation";
import { AlertTriangle, ArrowRightLeft, ScrollText, Warehouse } from "lucide-react";

const LINKS: { href: string; label: string; icon: typeof Warehouse }[] = [
  { href: "/inventory/stock", label: "Stock", icon: Warehouse },
  { href: "/inventory/movements", label: "Movements", icon: ScrollText },
  { href: "/inventory/transfers", label: "Transfers", icon: ArrowRightLeft },
  { href: "/inventory/alerts", label: "Low stock", icon: AlertTriangle },
  { href: "/inventory/approvals", label: "Adjustments", icon: ScrollText },
];

export function InventoryModuleNav() {
  const pathname = usePathname();
  return (
    <nav
      aria-label="Inventory"
      data-qa-inventory-module-nav
      className="flex flex-wrap items-center gap-1 border-b border-border pb-2"
    >
      {LINKS.map((link) => {
        const here = pathname === link.href || pathname?.startsWith(`${link.href}/`);
        const Icon = link.icon;
        return (
          <Link
            key={link.href}
            href={link.href}
            data-qa-inventory-module-link={link.href.split("/").pop()}
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
