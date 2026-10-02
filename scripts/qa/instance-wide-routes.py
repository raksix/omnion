#!/usr/bin/env python3
"""Derive the instance-wide API surface from the route registrations.

    python3 scripts/qa/instance-wide-routes.py <repo-root>

Prints one `METHOD /api/v1/...` line per route whose guard is an INSTANCE-WIDE permission --
the surface a delegated project owner must be refused on. Used by `run-delegated-admin.sh`.

## Why this is derived rather than written down

REQ-133 acceptance 10 says *"every instance-wide endpoint (users, settings, licence, other
projects) returns 403 for them from direct calls."* A gate that hard-codes that list is a
snapshot, and a snapshot rots **silently**: the day a writer adds `GET /iam/bindings`, the gate
keeps passing against the routes it remembers, and the acceptance line reads as proved on a
surface nobody looked at. The failure is invisible precisely because the gate is green.

So the list comes out of `apps/api/src/routes/mod.rs` — the one file that decides what is
registered and under which key. The caller still checks a hand-named floor (the routes the REQ
names) against it, which is what keeps the *derivation* honest rather than the *expectation*.

## What counts as instance-wide

A route is instance-wide when the permission guarding it is one the base role ladder never grants
to a non-instance role. Rather than maintaining a second list, the two facts that make a route
instance-wide are read from the same sources the platform reads them from:

* **`roles = BasePermissions::All`** — `owner` and `administrator` in
  `crates/permissions/src/seed.rs`. "All" means the catalogue, so the catalogue decides what
  they hold; a route whose key is in the catalogue is reachable by them.
* **explicitly-absent keys** — `projects.admin`, which the seed's own test asserts no enumerating
  base role carries, and which exists precisely to be the thing a delegated account does NOT get.

The practical rule this script implements, and the reason it is narrow: **a route is instance-wide
when its key is in a family the delegated role has no business in.** That is determined by
checking the key against the *member* role's granted list in `seed.rs` — the same enumeration the
platform's own seed gate uses. A key `member` holds (e.g. `crm.leads.read`) is not instance-wide;
a key it does not (e.g. `users.read`, `iam.audit.read`, `projects.admin`) is.

The `.merge(...)` / `.route(...)` shapes are handled by a brace-matching scan rather than a
regex, because axum nests one inside the other and a regex that stops at the first `)` silently
misses every method on a merged router.
"""

import re
import sys
from pathlib import Path

# Guard keys that are instance-wide by *definition* rather than by absence from `member`:
# they are the powers that override membership, and a route behind one is a delegated-account
# surface whatever the role ladder says.
INSTANCE_BY_DEFINITION = {
    "projects.admin",
}

# Path prefixes that are PROJECT-scoped rather than instance-wide, and why. Kept here rather
# than hidden in a heuristic because each entry is a decision somebody has to be able to audit.
#
# `/projects` and `/workflows` are guarded by `projects.*` / `workflows.*`, which a delegated
# project owner is *given* — that is the whole point of acceptance 10's first clause. Listing
# them would say "a project owner is refused on the project surface", which is the opposite of
# the sentence, and it would be a wrong claim rather than a weak one.
#
# What acceptance 10's "other projects" half actually means is *the store*, not a guard: a project
# the caller is not a member of answers 404 and the list omits it. Those are asserted directly by
# `run-delegated-admin.sh` against a real second project, which is the only thing that can answer
# them.
# `/workflow-executions` joins them for the same reason: it is guarded by `workflows.read` /
# `workflows.run`, keys the delegated project owner is *given*, and its 404 in the gate was not a
# refusal at all — it is a project-scoped read of a run row, answered after the guard by the store.
# The first version of the exclusion list read like a category ("automation") rather than a list
# of prefixes, and the row that was missing from it is the one that produced a 404 and a
# "derivation over-reached" note about a route the platform registers and guards correctly.
PROJECT_SCOPED = ("/projects", "/workflows", "/workflow-executions", "/automations")

# Which `{param}` names are UUIDs. Everything else gets `1`. Getting this wrong is a false pass:
# a path the router cannot parse is refused with 400 *before* the guard runs, so a 403 assertion
# would be measuring nothing at all.
_UUID_PARAMS = {"id", "user_id", "key_id", "factor_id", "site_id", "organization_id"}

# Paths that are not instance-wide even though a broad key guards them, with the reason. Kept
# short and kept here rather than hidden in a heuristic, because each entry is a decision somebody
# has to be able to audit.
EXEMPT = {
    # `/me` is the caller's own account, and every role needs it to render the panel at all.
    "/me",
    # The caller's own inbox. `notifications.read` is in `member` on purpose (the bell is in
    # the header of every screen) — the store scopes it to the caller's own rows.
    "/notifications",
}


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def member_role_keys(seed: str) -> set[str]:
    """The permission keys the `member` base role enumerates.

    Read out of the same `BASE_ROLES` table the seeder itself uses, by locating the `member`
    entry and pulling the strings out of its `BasePermissions::List(&[...])`.
    """
    start = seed.find('key: "member"')
    if start < 0:
        raise SystemExit("could not find the `member` base role in seed.rs")
    open_paren = seed.find("BasePermissions::List(&[", start)
    if open_paren < 0:
        # `member` is an enumerating role on this platform; if that ever stops being true the
        # derivation below would silently invert, so it is a hard error rather than a fallback.
        raise SystemExit("the `member` role is not an enumerating role — re-check the derivation")
    close = seed.find("])", open_paren)
    body = seed[open_paren:close]
    return set(re.findall(r'"([a-z][a-z0-9_.]*)"', body))


def all_permission_keys(catalogue: str) -> set[str]:
    """Every key in the permission catalogue, from `catalogue.rs`."""
    return set(re.findall(r'key:\s*"([a-z][a-z0-9_.]*)"', catalogue))


def _statement(text: str, start: int) -> tuple[str, int]:
    """The Rust expression beginning at `start`, up to its own `;`.

    Paren/bracket depth is counted, `;` inside a string literal is ignored, and a `//` comment
    runs to the end of its line. Anything less than that merges two statements into one binding.
    """
    depth = 0
    i = start
    in_str = False
    in_line_comment = False
    out = []
    while i < len(text):
        ch = text[i]
        if in_line_comment:
            if ch == "\n":
                in_line_comment = False
            out.append(ch)
            i += 1
            continue
        if in_str:
            out.append(ch)
            if ch == "\\":
                if i + 1 < len(text):
                    out.append(text[i + 1])
                    i += 2
                    continue
            elif ch == '"':
                in_str = False
            i += 1
            continue
        if ch == "/" and i + 1 < len(text) and text[i + 1] == "/":
            in_line_comment = True
        elif ch == '"':
            in_str = True
        elif ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch == ";" and depth == 0:
            return "".join(out), i
        out.append(ch)
        i += 1
    return "".join(out), len(text)


def after_path_of(body: str) -> str:
    """The route's handler expression, with the path argument removed.

    `body` is the text from the `.route(` OPENING PAREN, so it always begins with `("/path", …`.
    Matching an identifier pattern against the raw body never matches anything, and a check that
    can never fire is decoration — the same lesson as the tripwire in `catalogue.rs` that was
    never read. Returns the bare name when the body is exactly one identifier, else "".
    """
    after = re.sub(r'^\(\s*"[^"]*"\s*,?', "", body).strip().rstrip(",").strip()
    return after if re.fullmatch(r"[a-z_][a-z0-9_]*", after) else ""


def route_guards(mod: str) -> list[tuple[str, str, str]]:
    """`(path, method, guard_key)` for every guarded route in `routes/mod.rs`.

    ## Why a guard is paired with ONE method and not with every method in its route

    `.route("/projects/{id}", get(h).layer(require("projects.read")).merge(put(h).layer(require("projects.manage"))))`
    is two handlers with two different powers. Attributing both keys to both verbs produces four
    rows, two of which are **false claims** — "reading a project needs `projects.manage`" — and a
    false claim in a gate is worse than a missing one: it is a sentence nobody wrote on purpose.

    So the scan walks the route body in *source order* and pairs each `guards::require(…)` with
    the verb that precedes it. A key that appears before any verb (a guard wrapped around a whole
    sub-router rather than a handler) applies to every verb in the body, because that is what
    `.route_layer()` means.
    """
    found: list[tuple[str, str, str]] = []
    unknown_verbs: list[str] = []

    # Pass one: `let name = <router-expression>;` bindings. The IAM surface declares roughly
    # thirty of them and then attaches each to a path with `.route("/iam/users", iam_users)` --
    # the path and the guard are in *different statements*, and a scan that only looks at
    # `.route("…", <inline expression>)` therefore sees the path with **no guard at all** and
    # reports it as unguarded. That is the silent-shrink failure in its purest form: `/iam/users`
    # is the route the REQ names first, and it disappeared from the list without a word.
    bindings: dict[str, str] = {}
    # The expression ends at the FIRST `;` that is not inside a paren, a string or a comment.
    # A non-greedy `.*?;\n` sounds right and is not: the statement bodies here are
    # multi-line and some end with a comment, so it runs on and swallows the NEXT statement —
    # `workflow_execution` (a single `get`) picks up `workflow_execution_cancel`, `onboarding_owner`
    # and everything after, and the scan then reports GET+POST+PUT+PATCH+DELETE for a route
    # that has one method. The result is a list of false claims that looks rigorous.
    for bind in re.finditer(r'let\s+(\w+)\s*=\s*', mod):
        name = bind.group(1)
        expr, end = _statement(mod, bind.end())
        if expr and "guards::require" in expr:
            bindings[name] = expr

    # Offsets of every character that lives inside a `let … = …;` binding. A `.route(` that sits
    # inside one belongs to a SUB-router, and the guard that applies to it is the sub-router's
    # `.route_layer`, not the guard of whatever path the sub-router is later merged onto. Reading
    # it from the merged context invents permissions: `/crm/assignment/simulate` is guarded by
    # `crm.leads.read` inside its own router and by `crm.intake.manage` in the context that
    # merges it, and emitting both produced a row the product does not have.
    inside_binding: set[int] = set()
    for bind in re.finditer(r'let\s+\w+\s*=\s*', mod):
        expr, end = _statement(mod, bind.end())
        # ONLY sub-routers are excluded, and only when they carry their OWN `.route_layer`.
        # Excluding every `let` is catastrophic and looks correct for one tick: the main `v1`
        # router is itself a `let`, so the exclusion erases the entire API surface and the
        # script prints an empty list — which reads as "no instance-wide routes exist", the most
        # confident possible lie this file could emit.
        if "Router::new()" in expr and ".route_layer(" in expr:
            inside_binding.update(range(bind.end(), end))

    for match in re.finditer(r'\.route\(\s*\n?\s*"([^"]+)"', mod):
        if any(off in inside_binding for off in range(match.start(), match.end())):
            continue
        path = match.group(1)
        # Everything from this `.route(` to the matching close paren, brace-counted.
        depth = 0
        i = mod.index("(", match.start())
        end = i
        while end < len(mod):
            if mod[end] in "([":
                depth += 1
            elif mod[end] in ")]":
                depth -= 1
                if depth == 0:
                    break
            end += 1
        body = mod[i:end]

        # A body that carries no guard of its own but names a `let` binding from pass one is
        # `.merge(iam_users)` — the guard and the verb both live in the binding. Splicing the
        # binding's *whole expression* in is what makes the verbs right too: without it the
        # body has no `get(`/`post(` of its own, falls back to the `{"GET"}` default, and
        # `POST /iam/users` is reported as `GET /iam/users` — a wrong claim about the product
        # that still looks like a claim.
        # `\b` is not enough. In Rust `workflow_execution` is a PREFIX of `workflow_executions`
        # and of `workflow_execution_cancel`, and `\b` fires between `n` and `_` because `_` is a
        # word character only to the LEFT of the match — so the guard for `/workflows/{id}` got
        # spliced into `/workflow-executions/{id}`, producing PUT and DELETE rows for a route
        # that only answers GET. The name must be delimited on BOTH sides by a non-identifier
        # character, which is what `(?<![A-Za-z0-9_])…(?![A-Za-z0-9_])` says and what `\b` does
        # not.
        # The body is a bare identifier (usually) — so the binding to splice is the one whose
        # NAME IS THAT IDENTIFIER, looked up directly. Matching by "any binding mentioned in the
        # body" and taking the first hit is a dictionary-order coin toss: several bindings are in
        # scope, `workflows` comes before `workflow_execution`, and the guard for the OTHER
        # route gets spliced in. A substring match cannot tell them apart; an exact one can.
        if "guards::require" not in body:
            ident_name = after_path_of(body)
            if ident_name and ident_name in bindings:
                body = body + "\n/* binding */\n" + bindings[ident_name]

        # A `.route_layer(guards::require(…))` after a `.route(…)` inside a sub-router guards
        # the WHOLE router, and the route body above carries no key of its own. The scan's
        # "no key in this body" check then drops a route that is in fact guarded — a silent hole
        # in the surface, which is the failure this script exists to prevent. So the sub-router is
        # scanned as a unit: a `.route_layer` key is attributed to every route declared in the
        # same `Router::new()` block.
        layered = re.findall(r"\.route_layer\(\s*guards::require(?:_or_machine)?\(\s*&?state\s*,\s*\"([^\"]+)\"", body)

        # Tokenize the body in source order: either a guard key or a verb.
        tokens: list[tuple[str, str]] = []
        for token in re.finditer(
            r'guards::require(?:_or_machine)?\(\s*&?state\s*,\s*"([^"]+)"'
            r'|\b(get|post|put|patch|delete)\s*\(',
            body,
        ):
            if token.group(1):
                tokens.append(("key", token.group(1)))
            else:
                tokens.append(("verb", token.group(2).upper()))

        if not any(kind == "key" for kind, _ in tokens):
            if not layered:
                continue
            # Every verb this route really has, guarded by the router-wide key. `verbs` is
            # computed from the tokens below, so the pairing is done with the same code path.
            verbs_here = {value for kind, value in tokens if kind == "verb"} or {"GET"}
            for key in layered:
                for verb in verbs_here:
                    found.append((path, verb, key))
            continue
        # Two ways a route's verb is invisible from this file, and both are recorded rather
        # than guessed:
        #   * `.merge(some_router)` where the sub-router's verbs live in another statement.
        #   * the route body is a bare IDENTIFIER — `workflow_execution` in
        #     `.route("/workflow-executions/{id}", workflow_execution)` — which is a
        #     `MethodRouter` value assembled somewhere else (get+put+delete merged). There is
        #     no verb in the text, so the `{"GET"}` default invents one and the gate calls
        #     `DELETE /workflow-executions/{id}`, which axum answers `405` for.
        #
        # A `405` is not a small annoyance: it is a *claim about the product* that is false, and
        # in a list of 187 rows it is indistinguishable from a real refusal. The rows are
        # dropped and named on stderr, so the list is honestly a lower bound of the surface
        # rather than a wrong claim about all of it.
        bare_identifier = bool(after_path_of(body))
        if bare_identifier:
            # The verb — and usually the guard — live in a `let` elsewhere. The binding was
            # spliced in above; if it did not supply a verb either, the verb is genuinely not
            # knowable from this file and inventing `GET` would be a false claim.
            if not any(kind == "verb" for kind, _ in tokens):
                unknown_verbs.append(path)
                continue
        verbs = {value for kind, value in tokens if kind == "verb"} or {"GET"}
        leading = [value for kind, value in tokens if kind == "key"]
        last_verb: str | None = None
        for kind, value in tokens:
            if kind == "verb":
                last_verb = value
            elif last_verb is None:
                # A key declared before any verb guards the whole sub-router.
                for verb in verbs:
                    found.append((path, verb, value))
            else:
                found.append((path, last_verb, value))
        del leading  # read for clarity above; the pairing is what matters

    return found, unknown_verbs


def main() -> int:
    root = Path(sys.argv[1] if len(sys.argv) > 1 else ".").resolve()
    mod = read(root / "apps/api/src/routes/mod.rs")
    seed = read(root / "crates/permissions/src/seed.rs")
    catalogue = read(root / "crates/permissions/src/catalogue.rs")

    member = member_role_keys(seed)
    known = all_permission_keys(catalogue) | member | INSTANCE_BY_DEFINITION

    rows: set[tuple[str, str]] = set()
    found, unknown_verbs = route_guards(mod)
    # Report the rows whose verb the scan could not see, on stderr, so a caller reading the list
    # can see that the list is a lower bound rather than a complete surface.
    for path in sorted(set(unknown_verbs)):
        print(f"# verb not visible from this file, excluded: {path}", file=sys.stderr)

    for path, verb, key in found:
        if path in EXEMPT or any(path.startswith(e + "/") for e in EXEMPT):
            continue
        if any(path == p or path.startswith(p + "/") for p in PROJECT_SCOPED):
            continue
        if key not in known:
            # A guard key that is in no catalogue is a typo or a branch that has not landed;
            # it is not an instance-wide surface and guessing would be inventing a claim.
            continue
        if key in INSTANCE_BY_DEFINITION or key not in member:
            # Either a key that overrides membership by definition, or a key the smallest base
            # role does not hold: the two shapes a delegated account must be refused on.
            rows.add((verb, path))

    # Path parameters are substituted with a syntactically valid dummy of the right shape:
    # a UUID where the route wants one, `1` where it wants a number. The gate calls these URLs
    # and asserts the ANSWER is 403, so a 400 from a missing id would be a false pass — a router
    # that cannot parse a path refuses it before the guard ever runs.
    def shape(path: str) -> str:
        def one(m: re.Match) -> str:
            name = m.group(1)
            return "00000000-0000-4000-8000-000000000000" if name in _UUID_PARAMS else "1"
        return re.sub(r"\{([a-z_]+)\}", one, path)

    for verb, path in sorted(rows):
        print(f"{verb} /api/v1{shape(path)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
