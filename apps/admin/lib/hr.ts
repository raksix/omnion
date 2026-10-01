/**
 * The HR client: the eleven leave routes slice 2a shipped (REQ-055).
 *
 * It follows the inventory client's rule rather than inventing a fourth one: the panel owns the
 * transport, and what lives here is the module's own vocabulary — the `hr.leave.*` keys, the
 * day count **as a string**, and the two things a leave screen must never recompute for itself.
 *
 * ## Four things about this client that are deliberate
 *
 * * **A day count is a string, never a number.** The module trims `numeric(6,2)` once at the
 *   reader (`trim_scale`) so a row reads `3` and `0.5`, the way a person reads a leave balance.
 *   Parsing it into a JS number here would be a second place the value can be reshaped, and
 *   `0.1 + 0.2` is the reason the module ships text. `Days` is branded so a plain `number`
 *   cannot be passed where one is expected without a cast nobody will notice.
 * * **The preview is a route, and the form calls it.** "The number shown before submit equals the
 *   stored value" is an acceptance criterion; computing it in TypeScript would make it a second
 *   implementation and the criterion untestable. The working-day rule lives in Rust, once.
 * * **The calendar's `today` comes from the response.** The grid has to mark the same day the
 *   server considered current — a client computing its own would disagree with the window by
 *   however far the two clocks are apart — and the window bounds are the server's too, so the
 *   grid draws the month it was given rather than the month it assumes.
 * * **`can_decide` decides the decision panel, not the status.** A decided request offering an
 *   approve button is how a request ends up approved twice; the server tells the screen whether
 *   the panel belongs on the page at all.
 */
import { ApiError, type ErrorBody } from "./api";

/** A charged day count as the module sends it: `"3"`, `"0.5"`. */
export type Days = string & { readonly __days: unique symbol };

/** The four statuses a request can hold. */
export type LeaveStatus = "pending" | "approved" | "rejected" | "cancelled";

/** One page of leave requests. */
export type Page<T> = {
  items: T[];
  next_cursor: string | null;
  total_estimate: number;
};

/** A leave type from the catalogue. */
export type LeaveType = {
  id: string;
  organization_id: string;
  name: string;
  code: string;
  paid: boolean;
  /** The yearly entitlement, as the column's own text (`14.00`). */
  annual_days: string;
  requires_approval: boolean;
  allow_negative: boolean;
  active: boolean;
  created_at: string;
};

/** A leave type as the balance card flattens it in. */
export type BalanceCard = LeaveType & {
  employee_id: string;
  balance_year: number;
  entitled_days: Days;
  used_days: Days;
  pending_days: Days;
  remaining_days: Days;
  /** Whether the balance row existed. `false` = the type has never been taken. */
  seeded: boolean;
};

/** A request as the list reads it. */
export type LeaveRequest = {
  id: string;
  organization_id: string;
  employee_id: string;
  employee_name: string;
  leave_type_id: string;
  leave_type_name: string;
  starts_on: string;
  ends_on: string;
  days: Days;
  half_day: boolean;
  reason: string;
  leave_status: LeaveStatus;
  decided_by: string | null;
  decided_by_name: string | null;
  decided_at: string | null;
  decision_comment: string | null;
  cancelled_at: string | null;
  created_at: string;
};

/** One step of a request's history. */
export type TimelineStep = {
  kind: string;
  at: string;
  actor_id: string | null;
  comment: string | null;
};

/** A request with its balance, its timeline and whether it can still be decided. */
export type RequestDetail = LeaveRequest & {
  balance: BalanceCard;
  timeline: TimelineStep[];
  can_decide: boolean;
};

/** One absence bar on the calendar. */
export type AbsenceBar = {
  request_id: string;
  employee_id: string;
  employee_name: string;
  leave_type_id: string;
  leave_type_name: string;
  starts_on: string;
  ends_on: string;
  days: Days;
  /** The range runs past the window's last day. */
  continues_after: boolean;
  /** The range began before the window's first day. */
  continues_before: boolean;
};

/** One employee line of the calendar. */
export type AbsenceRow = {
  employee_id: string;
  employee_name: string;
  request_count: number;
};

/** The month grid: bounds, today, bars and rows — every bound the server owns. */
export type AbsenceCalendar = {
  from: string;
  to: string;
  today: string;
  bars: AbsenceBar[];
  employees: AbsenceRow[];
};

/** The working days a range would charge. */
export type DaysPreview = {
  days: Days;
  working_days: number;
};

/** The list's filters. Every one is optional; the server validates them. */
export type LeaveFilters = {
  status?: string;
  leave_type_id?: string;
  employee_id?: string;
  search?: string;
  from?: string;
  to?: string;
  pending_only?: boolean;
  limit?: number;
  cursor?: string;
  visibility?: string;
};

/** The body's fields of a new request. Omitted `employee_id` means the caller's own. */
export type NewLeaveRequest = {
  leave_type_id: string;
  starts_on: string;
  ends_on: string;
  half_day?: boolean;
  reason?: string;
  attachment_media_id?: string;
  employee_id?: string;
};

/** What a decision writes. */
export type Decision = {
  decision: "approved" | "rejected";
  comment?: string;
};

/** A `Response` that is not `ok`, turned into the platform's own error. */
async function readFailure(response: Response): Promise<ApiError> {
  const text = await response.text();
  let code = "unknown_error";
  let message = `The API answered with status ${response.status}.`;
  let details: Record<string, unknown> | null = null;
  let requestId: string | null = response.headers.get("x-request-id");
  try {
    const body = JSON.parse(text) as ErrorBody;
    code = body.error?.code ?? code;
    message = body.error?.message ?? message;
    details = body.error?.details ?? null;
    requestId = body.error?.request_id ?? requestId;
  } catch {
    // A non-JSON body is still an error; the status stays in the message.
  }
  return new ApiError(response.status, code, message, details, requestId);
}

/** One JSON call, with the same session, accept header and error shape the panel uses. */
async function hrRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: {
        accept: "application/json",
        ...(typeof init.body === "string" ? { "content-type": "application/json" } : {}),
        ...init.headers,
      },
    });
  } catch {
    throw new ApiError(0, "network_error", "The Omnion API could not be reached.");
  }
  if (!response.ok) {
    throw await readFailure(response);
  }
  const text = await response.text();
  if (!text) {
    return null as T;
  }
  return JSON.parse(text) as T;
}

/** Build a query string, dropping the empties so a cleared filter is not sent as `?search=`. */
function query(filters: Record<string, unknown> | undefined): string {
  const parts: string[] = [];
  for (const [key, value] of Object.entries(filters ?? {})) {
    if (value === undefined || value === null || value === "") {
      continue;
    }
    parts.push(`${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`);
  }
  return parts.length === 0 ? "" : `?${parts.join("&")}`;
}

/** `GET /hr/leave/types` — the catalogue. */
export function fetchLeaveTypes(): Promise<{ items: LeaveType[] }> {
  return hrRequest<{ items: LeaveType[] }>("/api/v1/hr/leave/types");
}

/** `POST /hr/leave/types` — add a type. Needs `hr.leave.manage`. */
export function createLeaveType(
  type: Pick<LeaveType, "name" | "code"> &
    Partial<Pick<LeaveType, "paid" | "annual_days" | "requires_approval" | "allow_negative" | "active">>,
): Promise<LeaveType> {
  return hrRequest<LeaveType>("/api/v1/hr/leave/types", {
    method: "POST",
    body: JSON.stringify(type),
  });
}

/** `PATCH /hr/leave/types/{id}` — edit a type. Needs `hr.leave.manage`. */
export function updateLeaveType(
  id: string,
  changes: Partial<
    Pick<
      LeaveType,
      "name" | "code" | "paid" | "annual_days" | "requires_approval" | "allow_negative" | "active"
    >
  >,
): Promise<LeaveType> {
  return hrRequest<LeaveType>(`/api/v1/hr/leave/types/${id}`, {
    method: "PATCH",
    body: JSON.stringify(changes),
  });
}

/** `GET /hr/leave/balances` — every type's card for one employee in one year. */
export function fetchBalances(employeeId?: string, year?: number): Promise<{ items: BalanceCard[] }> {
  return hrRequest<{ items: BalanceCard[] }>(
    `/api/v1/hr/leave/balances${query({ employee_id: employeeId, year })}`,
  );
}

/** `GET /hr/leave/requests` — a page of requests. */
export function fetchRequests(filters: LeaveFilters = {}): Promise<Page<LeaveRequest>> {
  return hrRequest<Page<LeaveRequest>>(
    `/api/v1/hr/leave/requests${query(filters as Record<string, unknown>)}`,
  );
}

/** `GET /hr/leave/requests/preview` — the days a range would charge. The form's day counter. */
export function previewDays(
  starts_on: string,
  ends_on: string,
  half_day?: boolean,
): Promise<DaysPreview> {
  return hrRequest<DaysPreview>(
    `/api/v1/hr/leave/requests/preview${query({ starts_on, ends_on, half_day })}`,
  );
}

/** `GET /hr/leave/requests/{id}` — detail with balance, timeline and `can_decide`. */
export function fetchRequest(id: string): Promise<RequestDetail> {
  return hrRequest<RequestDetail>(`/api/v1/hr/leave/requests/${id}`);
}

/** `POST /hr/leave/requests` — raise one. Omitted `employee_id` means the caller's own. */
export function createRequest(body: NewLeaveRequest): Promise<LeaveRequest> {
  return hrRequest<LeaveRequest>("/api/v1/hr/leave/requests", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

/** `POST /hr/leave/requests/{id}/decision` — approve or reject. Needs `hr.leave.approve`. */
export function decideRequest(id: string, decision: Decision): Promise<LeaveRequest> {
  return hrRequest<LeaveRequest>(`/api/v1/hr/leave/requests/${id}/decision`, {
    method: "POST",
    body: JSON.stringify(decision),
  });
}

/** `POST /hr/leave/requests/{id}/cancel` — withdraw a pending request. */
export function cancelRequest(id: string): Promise<LeaveRequest> {
  return hrRequest<LeaveRequest>(`/api/v1/hr/leave/requests/${id}/cancel`, {
    method: "POST",
    body: "{}",
  });
}

/** `GET /hr/leave/calendar` — the month grid. The bounds and `today` are the server's. */
export function fetchCalendar(params: { from?: string; to?: string } = {}): Promise<AbsenceCalendar> {
  return hrRequest<AbsenceCalendar>(`/api/v1/hr/leave/calendar${query(params)}`);
}
