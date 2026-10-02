#!/usr/bin/env python3
"""Adopt `documented!` across the router, resolving each route's permission from the guard
BESIDE it and never inventing one (REQ-130, slice 3).

Why a script and not a by-hand edit: there are 402 `.route(` registrations in one 2 650-line
function, and the one thing that must not go wrong is the permission. A permission that is not
the catalogue key the guard already uses answers `403` for everyone including the instance
owner -- the route looks installed and serves nobody. A mechanical pass that copies the key from
the guard beside the handler cannot invent one, because it never has one to begin with.

The shapes it resolves:
  * inline method calls      `.route("/x", get(h).layer(guards::require(&state, "k")))`
  * inline multi-method      `.route("/x", get(h).merge(post(h).layer(...)))`
  * bound routers            `.route("/x", binding)` where the binding holds the guard
  * route_layer routers      permission inherited from a `route_layer(guards::require(..))` on
                             the same `Router::new()` chain -- INHERITED, never guessed
  * truly unguarded routes    recorded with an empty permission, which the inventory stores as
                             `None` rather than as a key that would refuse everybody

Anything it cannot resolve is reported and left alone: a route nobody can read is safer than a
route given a key that does not exist.
"""
import glob
import re
import sys

PATH = "apps/api/src/routes/mod.rs"
VERB = r'(get|post|put|patch|delete|head|options|trace)'

def mask_comments(src):
    """Blank out comments, preserving every byte offset, so the scanners below cannot match a
    `.route(` or a permission key that only exists in prose.

    String literals are tracked, so a `//` inside a path like `"https://x"` survives -- masking it
    would corrupt the very text the scan is reading.
    """
    out = list(src)
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c == '"':
            i += 1
            while i < n:
                if src[i] == '\\':
                    i += 2
                    continue
                if src[i] == '"':
                    break
                i += 1
        elif src.startswith('//', i):
            j = src.find('\n', i)
            j = n if j < 0 else j
            for k in range(i, j):
                out[k] = ' '
            i = j
        elif src.startswith('/*', i):
            j = src.find('*/', i + 2)
            j = n if j < 0 else j + 2
            for k in range(i, j):
                if out[k] != '\n':
                    out[k] = ' '
            i = j
        i += 1
    return ''.join(out)

def find_calls(src, token):
    """(start, end, body) for each token call, skipping string literals so a ')' inside a
    path or comment does not close the scan early."""
    out, i = [], 0
    while True:
        i = src.find(token, i)
        if i < 0:
            return out
        j, depth = i + len(token), 1
        while depth and j < len(src):
            c = src[j]
            if c == '"':
                j += 1
                while j < len(src) and src[j] != '"':
                    j += 2 if src[j] == '\\' else 1
            elif c == '(':
                depth += 1
            elif c == ')':
                depth -= 1
            j += 1
        out.append((i, j, src[i + len(token):j - 1]))
        i = j

def lets_of(src):
    """`let NAME = EXPR;` including multi-line initialisers.

    A line-anchored regex misses `let x =\n    Router::new()\n        .route(..)`, which is how the
    larger routers in this file are written, and an unresolved binding silently becomes an
    unhandled shape.
    """
    out = {}
    for m in re.finditer(r'\blet\s+([a-z_][a-z0-9_]*)\s*(?::[^=]+)?=\s*', src):
        name = m.group(1)
        j, depth, in_str = m.end(), 0, False
        while j < len(src):
            c = src[j]
            if in_str:
                if c == '\\':
                    j += 2
                    continue
                if c == '"':
                    in_str = False
            elif c == '"':
                in_str = True
            elif c in '([{':
                depth += 1
            elif c in ')]}':
                depth -= 1
            elif c == ';' and depth == 0:
                break
            j += 1
        out[name] = src[m.end():j].strip()
    return out

def guard_of(text):
    """The catalogue key of a `guards::..` call appearing in `text`, or None.

    The KEY is the LAST argument, raw. Two mistakes this had, both found by running it against
    the real file:

    * Matching `[^,]*` for the state is wrong: the call is `guards::require(&state, "media.read")`,
      so the first comma ends the state argument and every `require` route reads as unguarded.
      That is how 337 routes ended up in the first draft of this file.
    * A greedy `(.*)\)` over the WHOLE sub-expression instead of the call's own parentheses ran
      past the `guards::require(&state, "x.y")` into the next `..get(` and returned `x.y")).get(`
      as a permission. The arguments are therefore scanned from a balanced-paren scan of the
      `guards::` call itself, not from the enclosing text.
    """
    m = re.search(r'guards::[a-z_]+\s*', text)
    if not m:
        return None
    open_paren = text.find('(', m.end())
    if open_paren < 0:
        return None
    depth, j, in_str = 1, open_paren + 1, False
    while j < len(text) and depth:
        c = text[j]
        if in_str:
            if c == '\\':
                j += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
        j += 1
    if depth:
        return None
    args = text[open_paren + 1:j - 1]
    tail = args.rsplit(',', 1)[-1].strip()
    if tail.startswith('"') and tail.endswith('"'):
        return tail[1:-1]
    # A bare symbol is a const, and a quoted key is a literal -- `"analytics.read"` LOOKS like a
    # dotted path and would be sent to `resolve_const`, which returns None for it, so every
    # ordinary permission in the file silently read as absent. The quotes decide it.
    return resolve_const(tail)

def resolve_const(token):
    """`module::CONST` -> the string literal it holds, searched across the route module."""
    if not token.startswith('graphql_manager::'):
        return None
    name = token.split('::')[-1]
    for path in glob.glob('apps/api/src/routes/*.rs'):
        text = open(path).read()
        m = re.search(r'const\s+' + name + r'\s*:\s*&str\s*=\s*"([^"]+)"', text)
        if m:
            return m.group(1)
    return None

VERB_RE = r'(?<![a-z_:])(?:axum::routing::)?' + VERB + r'\s*\('
"""A verb call in chain position.

The lookbehind rejects `:` so `axum::routing::patch(` is not matched twice, and it rejects
identifier characters so `target(` never reads as `get(`. It must NOT reject `.`: the chain form
`.get(h)` IS the case this pass exists to detect, and excluding it was why every merged router
came back as one verb.
"""

def split_routers(expr):
    """Split a MethodRouter expression into one sub-expression per verb, so each can be wrapped
    in its own `documented!` with its own permission.

    The sub-expressions REJOIN to the original by concatenation, and that round-trip is the
    property that makes this safe: a handler rebuilt from verb names instead of preserved verbatim
    passes a compile it should never have passed, because `documented!(Method::GET, path, get, ..)`
    is a valid-looking macro call that hands axum the *function* instead of the router.

    A verb call is a CUT POINT when nothing binds it to a preceding expression -- which is what
    "depth zero between the two verb calls" means here. Two mistakes, both found by running it:

    * The first verb in an expression has NO preceding bracket, so a scan that only breaks at
      `(` silently refuses to cut there and the whole expression stays one part. Then
      `post(h).layer(g).get(h2)` reads as a single POST whose permission is `g` -- the GET loses
      its own route and its own guard.
    * A verb INSIDE another call's parentheses -- `merge(get(h))`, a handler's own argument -- is
      not a chain element and must not be cut.
    """
    hits = list(re.finditer(VERB_RE, expr))
    if not hits:
        return [expr]
    cuts = [h.start() for h in hits if h.start() == 0 or _is_chain_boundary(expr, h.start())]
    if not cuts:
        return [expr]
    # The `.merge(` between two verbs belongs to the CHAIN, not to the verb before it, and the
    # verb after it is missing its own opening bracket. Each part is therefore closed to a
    # standalone router: strip a trailing `.merge(` and add the `)` that merge's argument consumed.
    #
    # Without both halves the emitted `documented!` wraps a dangling `.merge(` and the file does
    # not parse -- which is the good kind of failure, and the reason the round-trip assertion in
    # `route-adoption.test.cjs` is checked before anything is written.
    parts, prev = [], 0
    for c in cuts:
        chunk = expr[prev:c]
        # The `.merge(` operator belongs to the chain, not to a verb: drop it, and drop the single
        # `)` that closed merge -- which lands immediately after the verb call it wrapped. That
        # paren is what produced `{ post(commands::record)) }` and a file that does not parse.
        parts.append(_strip_merge(chunk))
        prev = c
    parts.append(_strip_merge(expr[prev:]))
    return [p for p in parts if p.strip(', ')]

def _strip_merge(chunk):
    """Remove a trailing `.merge(` and the `)` that closed it, leaving a standalone router.

    The trailing DOT is dropped too. A chain element that follows another one begins where its
    method name begins -- `.delete(h)` contributes from `delete`, not from the dot -- so the dot
    belongs to nothing once the two verbs are in separate parts. Left in, it is what produced
    `{ axum::routing::patch(..)\n . }` and *unexpected token: `}`*.
    """
    out = re.sub(r'\.merge\(\s*$', '', chunk).rstrip()
    # a `)` sitting directly after a completed top-level verb call is merge's, not ours
    while True:
        cut = _merge_close_at(out)
        if cut is None:
            break
        out = out[:cut] + out[cut + 1:]
    return out.strip().rstrip('.').strip()

def _merge_close_at(chunk):
    """Index of the `)` that closes an enclosing `.merge(`, or None.

    Three spellings occur in this router and the first version only handled one:

    * `get(h).merge(post(x))`      -- merge's `)` directly follows the verb's
    * `.merge(\n  post(x),\n)`     -- a trailing comma and newlines before the `)`
    * `.merge(post(x).layer(g),)`  -- a trailing comma with no newline

    All three are the same thing: one extra closing paren after a completed verb expression.
    """
    depth, i, in_str, after_verb = 0, 0, False, False
    while i < len(chunk):
        c = chunk[i]
        if in_str:
            if c == '\\':
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c in '([{':
            if depth == 0 and VERB_HIT(chunk, i):
                after_verb = True
            depth += 1
        elif c in ')]}':
            depth -= 1
            if depth == 0:
                after_verb = True
            # a completed verb expression followed by `,?` and then `)` is merge's paren
            if depth == 0 and after_verb and c == ')':
                j = i + 1
                while j < len(chunk) and chunk[j] in ' \t\n':
                    j += 1
                if j < len(chunk) and chunk[j] in '),':
                    return i + 1
                after_verb = False
        i += 1
    return None

def VERB_HIT(chunk, at):
    """Whether an opening bracket at `at` opens a routing verb call."""
    e = at - 1
    while e >= 0 and chunk[e] in ' \t\n':
        e -= 1
    f = e
    while f >= 0 and (chunk[f].isalnum() or chunk[f] in '_:'):
        f -= 1
    name = chunk[f + 1:e + 1].split('::')[-1]
    return name in ('get', 'post', 'put', 'patch', 'delete', 'head', 'options', 'trace')

def _parens(s):
    """Net parenthesis depth of `s`, string-aware."""
    depth, in_str = 0, False
    i = 0
    while i < len(s):
        c = s[i]
        if in_str:
            if c == '\\':
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == '(':
            depth += 1
        elif c == ')':
            depth -= 1
        i += 1
    return depth

def _is_chain_boundary(expr, at):
    """Whether the verb at `at` starts a new chain element rather than nesting inside a call."""
    i, depth, in_str = at - 1, 0, False
    while i >= 0:
        c = expr[i]
        if in_str:
            if c == '\\':
                i -= 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c in ')]}':
            depth += 1
        elif c in '([{':
            if depth == 0:
                # The verb binds to whatever precedes this bracket. Walk back over that binding:
                # an identifier, and the `.name` of a method call such as `.layer(` -- so
                # `post(h).layer(g).get(h2)` sees `layer` and cuts, while `f(get(h))` sees a bare
                # function name and does not.
                k = i - 1
                while k >= 0 and expr[k] in ' \t\n':
                    k -= 1
                if k < 0:
                    return True
                if expr[k].isalnum() or expr[k] in '_)]"':
                    # the name that owns this bracket
                    e = k
                    while e >= 0 and (expr[e].isalnum() or expr[e] == '_'):
                        e -= 1
                    name = expr[e + 1:k + 1]
                    # `.merge(x)` takes a ROUTER as its argument, so a verb inside it is a
                    # separate router on the same path, not an argument of the outer one. Every
                    # other call takes a handler or a layer, where a nested verb is not a router.
                    if name == 'merge':
                        return True
                    k = e - 1
                    while k >= 0 and expr[k] in ' \t\n':
                        k -= 1
                    if k < 0:
                        return True
                    if expr[k] == '.':
                        return True
                    if expr[k] in ')]':
                        continue        # the binding is itself a call result; keep walking left
                    return False       # `identifier(` -- an argument, not a chain element
                return False
            depth -= 1
        i -= 1
    return depth == 0

def guard_covers_all(expr):
    """Whether a `guards::` call guards the WHOLE router or only the leg it is written on.

    `owner == 'layer'` is true in BOTH shapes, so it cannot be the discriminator -- that was the
    first version, and it documented the GET leg of `get(h).merge(delete(h2).layer(g))` with the
    DELETE's permission. The discriminator is DEPTH:

    * `get(recent).merge(post(record)).merge(delete(clear)).layer(g)` -- every merge is closed
      before the layer, so the layer sits at depth 1 and chains onto the merged router: `g` guards
      all three verbs.
    * `get(h).merge(delete(h2).layer(g))` -- the layer is inside merge's parentheses, at depth 2,
      and guards the DELETE leg alone.

    Both are real shapes in this repository, and reading the second as the first (or the reverse)
    misstates a permission the guard does not enforce.
    """
    for m in re.finditer(r'guards::[a-z_]+\s*', expr):
        if enclosing_call(expr, m.start()) != 'layer':
            continue
        if _paren_depth(expr, m.start()) == 1:
            return True
    return False

def enclosing_call(expr, at):
    """Name of the INNERMOST call whose parentheses contain `at`, or None.

    "Innermost" is the whole point and the reason the first version was wrong: a leftward scan
    that keeps skipping brackets reaches `.merge(` for a guard written as
    `get(h).merge(delete(h2).layer(g))`, so the DELETE's guard read as a whole-router guard and
    the GET was documented with a permission that does not apply to it. The nearest enclosing
    bracket is `.layer(`, which is the owner that matters.
    """
    stack = []
    i, in_str = 0, False
    while i < at and i < len(expr):
        c = expr[i]
        if in_str:
            if c == '\\':
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c in '([{':
            stack.append((c, i))
        elif c in ')]}':
            if stack:
                stack.pop()
        i += 1
    if not stack:
        return None
    _open, idx = stack[-1]
    e = idx - 1
    while e >= 0 and expr[e] in ' \t\n':
        e -= 1
    f = e
    while f >= 0 and (expr[f].isalnum() or expr[f] == '_'):
        f -= 1
    return expr[f + 1:e + 1] or None

def top_level_guard(expr):
    """The key of a `guards::..` call that guards the whole expression, or None."""
    for m in re.finditer(r'guards::[a-z_]+\s*', expr):
        if enclosing_call(expr, m.start()) == 'layer':
            return guard_of(expr[m.start():])
    return None

def _paren_depth(expr, at):
    depth, in_str = 0, False
    i = 0
    while i < at and i < len(expr):
        c = expr[i]
        if in_str:
            if c == '\\':
                i += 2
                continue
            if c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c in '([{':
            depth += 1
        elif c in ')]}':
            depth -= 1
        i += 1
    return depth

def methods_of(expr):
    """(verb, permission, sub_expression) per registered verb, in source order.

    Resolution order, and the order is the contract:

    1. a `guards::..` key inside the verb's OWN sub-expression -- `.merge` of two keyed handlers;
    2. a `route_layer` key inherited from the enclosing chain (applied by the caller);
    3. otherwise a depth-zero trailing `.layer(guards::..)`, when it is CHAINED onto the router
       rather than passed as `merge`'s argument, in which case it covers every verb in the chain.

    (3) is why the parts carry the tail rather than being read independently: `post(h).get(h2)
    .layer(g)` is two verbs and ONE guard, and the guard sits in the part that contains it -- the
    last -- so the POST, equally guarded, came out documented unguarded.
    """
    found = []
    parts = split_routers(expr)
    shared = top_level_guard(expr) if guard_covers_all(expr) else None
    for part in parts:
        m = re.search(VERB_RE, part)
        if not m:
            continue
        verb = m.group(0).split('(')[0].split('::')[-1].strip()
        key = guard_of(part) or shared
        found.append((verb.upper(), key, part.strip().rstrip(',').strip()))
    return found

def route_layer_keys(src):
    """For each `.route(` call, the key of the `route_layer` that actually applies to it.

    The key is found by PARSING THE CHAIN, not by scanning backwards for a token, and both
    naive versions were wrong against the real file:

    * The layer sits at the CHAIN'S TAIL -- `Router::new().route(..).route(..).route_layer(..)`
      -- so reading only what precedes the route misses it.
    * "Nearest `Router::new()` backwards" is not the chain. `let observability_read = Router::new()
      ...` runs for three hundred lines, and a LATER `Router::new()` belonging to another variable
      sits between two of its routes.
    * And taking the enclosing STATEMENT is worse than both. The `v1` router is one statement with
      a `route_layer` on its tail, so every route in the file resolves to that one key -- 337
      routes documented with the same permission, which is a document that lies about 337
      operations and passes every check in the gate.

    So the chain is walked properly: from each `Router::new()`, consume `.method(..)` calls while
    the next token is a dot, and take the LAST `route_layer` inside that span. A route inside a
    nested `Router::new()` belongs to the INNER chain, so spans are searched innermost-first.
    """
    keys = {}
    spans = sorted(router_chains(src), key=lambda s: s[0] - s[1])  # innermost (shortest) first
    for chain_start, chain_end, layer in spans:
        if layer is None:
            continue
        for start, _end, _body in find_calls(src, '.route('):
            if start < chain_start:
                continue
            if start > chain_end:
                break
            keys[start] = layer
    return keys

def router_chains(src):
    """(start, end, route_layer_key_or_None) for every `Router::new()` chain in the file."""
    out = []
    for m in re.finditer(r'Router::new\(\)', src):
        j = m.end()
        layer = None
        while True:
            k = j
            while k < len(src) and src[k] in ' \t\n':
                k += 1
            if k >= len(src) or src[k] != '.':
                break
            name = re.match(r'\.([a-z_][a-z0-9_]*)', src[k:])
            if not name:
                break
            open_paren = k + name.end()
            depth, j = 1, open_paren + 1
            while depth and j < len(src):
                c = src[j]
                if c == '"':
                    j += 1
                    while j < len(src) and src[j] != '"':
                        j += 2 if src[j] == '\\' else 1
                elif c == '(':
                    depth += 1
                elif c == ')':
                    depth -= 1
                j += 1
            if name.group(1) == 'route_layer':
                found = guard_of(src[open_paren:j - 1])
                if found:
                    layer = found
            # `.route_layer(...)` may be followed by `.with_state(..)`; keep walking.
        out.append((m.end(), j, layer))
    return out

def annotate_bindings(src):
    """Give every handler binding an explicit `MethodRouter<AppState, Infallible>` type.

    ## Why this exists at all

    A `MethodRouter`'s second type parameter is the handler's error type, and axum's builders
    leave it to inference at the point where the router is USED. Before `documented!`, that point
    was the `.route(..)` call, which passed the router into a typed `Router<AppState>` and settled
    the parameter. Inside the macro there is no such use -- the macro's body constrains nothing --
    so the inference vanished and 161 bindings failed with

        E0283: type annotations needed for `MethodRouter<AppState, _>`

    **reported at `let roles = get(..)` -- a line this pass does not touch.** That is what made it
    hard to read: the error is in the binding, the cause is in the call site, and there are 161 of
    them.

    Annotating is not a workaround; it restates the type the use site used to infer, and it makes
    the binding's contract visible where the binding is declared. 57 bindings already carry it.
    """
    masked = mask_comments(src)
    verb_start = re.compile(
        r'^\s*(?:axum::routing::)?(?:get|post|put|patch|delete|head|options|trace)\s*\('
    )
    out = src
    edits = []
    for mm in re.finditer(r'\blet\s+([a-z_][a-z0-9_]*)\s*(:\s*[^=]+?)?=\s*', masked):
        name, ann = mm.group(1), mm.group(2)
        if ann:
            continue
        j, depth, in_str = mm.end(), 0, False
        while j < len(masked):
            c = masked[j]
            if in_str:
                if c == '\\':
                    j += 2
                    continue
                if c == '"':
                    in_str = False
            elif c == '"':
                in_str = True
            elif c in '([{':
                depth += 1
            elif c in ')]}':
                depth -= 1
            elif c == ';' and depth == 0:
                break
            j += 1
        if verb_start.match(masked[mm.end():j]):
            # The annotation goes after the NAME and before whatever the regex matched next --
            # which may be a type, or ` = `. Using `mm.end(1) + len(name)` was right for the
            # untyped case and wrong for `let roles: MethodRouter = ..`, where it landed the
            # annotation after the first character of the type: `let roles: M` became
            # `let roles: M` with a second `: ...` spliced into `get(..)` as `ge: T)t(..)`.
            edits.append((mm.end(1), mm.end(1)))
    for name_end in sorted({e[0] for e in edits}, reverse=True):
        out = out[:name_end] + ': MethodRouter<AppState, Infallible>' + out[name_end:]
    return out, len(edits)

def main():
    src = open(PATH).read()
    original = src
    masked = mask_comments(src)
    lets = lets_of(masked)
    rl = route_layer_keys(masked)
    verbs_with = lambda e: methods_of(e)

    edits = []          # (start, end_of_handler_arg, replacement)
    unresolved = []
    counts = {'inline': 0, 'multi': 0, 'bound': 0, 'inherited': 0, 'unguarded': 0}

    for start, end, body in find_calls(masked, '.route('):
        mp = re.match(r'\s*"([^"]*)"\s*,(.*)$', body, re.S)
        if not mp:
            unresolved.append((start, 'no literal path', body[:60]))
            continue
        path, rest = mp.group(1), mp.group(2).strip()
        if rest.endswith(','):
            rest = rest[:-1].strip()

        # locate the handler argument's own span inside the ORIGINAL text, by offset
        inner_off = body.index(rest, mp.start(2))
        h_start = start + len('.route(') + inner_off
        h_end = h_start + len(rest)

        if re.fullmatch(r'[a-z_][a-z0-9_]*', rest):
            expr = lets.get(rest)
            if expr is None:
                unresolved.append((start, 'unresolved binding', rest))
                continue
            methods = verbs_with(expr)
            if not methods:
                unresolved.append((start, 'binding has no verb', rest))
                continue
            counts['bound'] += 1
        elif re.match(r'^\s*' + VERB + r'\s*\(', rest) or re.match(r'^\s*axum::routing::' + VERB, rest):
            methods = verbs_with(rest)
            if not methods:
                unresolved.append((start, 'no verb in inline', rest[:60]))
                continue
            counts['inline'] += 1
        else:
            # `binding.merge(binding,)` — two already-built routers merged at the route call,
            # so each side's verbs AND each side's guard come from its own binding.
            sides = [s.strip() for s in re.split(r'\.merge\(', rest) if s.strip()]
            methods = []
            resolvable = True
            for side in sides:
                # the split leaves the call's closing paren on every side but the first
                side = side.rstrip(')').rstrip(',').strip()
                if not re.fullmatch(r'[a-z_][a-z0-9_]*', side):
                    resolvable = False
                    break
                expr = lets.get(side)
                found = verbs_with(expr) if expr else []
                if not found:
                    resolvable = False
                    break
                methods.extend(found)
            if not resolvable or not methods:
                unresolved.append((start, 'unhandled second argument shape', rest[:70]))
                continue
            counts['bound'] += 1

        perms = [p for _, p, _ in methods if p]
        inherited = rl.get(start)
        if not perms and inherited:
            counts['inherited'] += 1
        if not perms:
            counts['unguarded'] += 1

        # one documented! per (path, verb), each wrapping its ORIGINAL sub-expression unchanged
        #
        # The handler goes inside BRACES. `documented!` takes it as a `block` because a guard
        # carries a top-level comma -- `guards::require(&state, "media.read")` -- and an `expr`
        # fragment ends at that comma, shifting every later argument one position left. That
        # failure is late and unreadable: *missing tokens in macro arguments*, on a line that
        # looks right. Wrapping is not optional formatting.
        #
        # The TYPE is not annotated here: a macro `block` fragment matches one `{ expression }`,
        # so a `let` inside the braces does not parse. `documented!` settles the handler's type
        # itself, which is also the only place it can -- see the macro's own comment.
        parts = []
        for verb, perm, handler in methods:
            key = perm or inherited or ''
            parts.append(
                'documented!(\n                Method::%s,\n                "%s",\n'
                '                { %s },\n'
                '                "%s",\n                "%s"\n            )'
                % (verb, path, handler, key, summary_for(path, verb))
            )
        if len(parts) == 1:
            replacement = parts[0]
        else:
            # CHAIN the merges: `a.merge(b).merge(c)`. The first attempt nested them as further
            # arguments -- `.merge(a, b)` -- because the join put each next part inside the
            # previous call's parentheses. `MethodRouter::merge` takes ONE router, so that is
            # neither what axum accepts nor what the router served before, and the error it
            # produced (`mismatched closing delimiter` at the whole `router()` fn) pointed at the
            # function rather than at the call that was wrong.
            head, tail = parts[0], parts[1:]
            body = head
            for p in tail:
                body += '\n            .merge(' + p.strip() + ')'
            replacement = body
        edits.append((h_start, h_end, replacement))

    print("resolved:", counts)
    print("unresolved:", len(unresolved))
    for u in unresolved[:25]:
        print("   ", u)
    if '--apply' not in sys.argv:
        print("dry run; pass --apply")
        return 0

    for h_start, h_end, replacement in sorted(edits, reverse=True):
        src = src[:h_start] + replacement + src[h_end:]

    # Second phase, AFTER the routes: the bindings must be annotated against the text the route
    # rewrite produced, so an offset measured before it would land in the wrong place.
    src, annotated = annotate_bindings(src)
    open(PATH, 'w').write(src)
    print("wrote", PATH, "route edits:", len(edits), "bindings annotated:", annotated)
    return 0

def summary_for(path, verb):
    tail = path.rstrip('/').split('/')[-1] or 'root'
    tail = tail.split('?')[0].strip('{}').replace('-', ' ').replace('_', ' ')
    return ('%s %s' % (verb, tail)).strip()

if __name__ == '__main__':
    sys.exit(main())