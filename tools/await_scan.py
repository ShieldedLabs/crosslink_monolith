#!/usr/bin/env python3
"""Find async code that never awaits.

Usage:
    python3 tools/await_scan.py [ROOT] [--tests]

ROOT defaults to the current directory. Test code (files under tests/, files with
"test" in their name, `#[cfg(test)]` modules and `#[test]` functions) is skipped
unless --tests is given.

It reports two shapes:

- `async fn` bodies with no `.await` of their own, grouped by where they sit.
  Clippy's `unused_async` already finds the free and inherent ones, but it skips
  methods in trait impls, because the trait fixes their signature. Those are the
  ones this script exists for: they are usually a sync body behind an async trait
  (jsonrpsee, tonic), and could move into a sync fn the trait method calls.
- `async { .. }` blocks with no `.await`: a future that is ready as soon as it is
  polled. Many of them inside one `impl Service` mean the service is synchronous
  underneath, with tower's `call` wrapping each answer in a future.

This is a token-level heuristic, not a Rust parser. `join!`, `try_join!`,
`select!` and `select_biased!` count as awaits, because they await without an
`.await` token. An await inside a nested async block, async closure or nested fn
does not count toward the body around it. Code behind a disabled `cfg` is
scanned like any other.

It cannot find the larger class: functions whose awaits all land on services
that are synchronous underneath. Those show up here only once their callees are
made sync, so rerun it after each such step.
"""
import argparse
import os
import re
import sys
from collections import defaultdict

IDENT = re.compile(r'[A-Za-z_][A-Za-z0-9_]*')
AWAITING_MACROS = ('join', 'try_join', 'select', 'select_biased')


def tokenize(src):
    """Return (text, line) tokens with comments dropped and literals collapsed."""
    toks = []
    i, n, line = 0, len(src), 1
    while i < n:
        c = src[i]
        if c == '\n':
            line += 1
            i += 1
            continue
        if c.isspace():
            i += 1
            continue
        if src.startswith('//', i):
            j = src.find('\n', i)
            i = n if j < 0 else j
            continue
        if src.startswith('/*', i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith('/*', j):
                    depth += 1
                    j += 2
                elif src.startswith('*/', j):
                    depth -= 1
                    j += 2
                else:
                    if src[j] == '\n':
                        line += 1
                    j += 1
            i = j
            continue
        m = re.match(r'b?r(#*)"', src[i:i + 10])
        if m:
            hashes = m.group(1)
            end = src.find('"' + hashes, i + len(m.group(0)))
            end = n if end < 0 else end + 1 + len(hashes)
            line += src.count('\n', i, end)
            toks.append(('"str"', line))
            i = end
            continue
        if c == '"' or (c == 'b' and i + 1 < n and src[i + 1] == '"'):
            j = i + (2 if c == 'b' else 1)
            while j < n and src[j] != '"':
                if src[j] == '\\':
                    j += 1
                j += 1
            line += src.count('\n', i, j)
            toks.append(('"str"', line))
            i = j + 1
            continue
        if c == "'":
            m = re.match(r"'(\\(x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.)|[^\\'])'", src[i:i + 12])
            if m:  # char literal
                toks.append(("'c'", line))
                i += len(m.group(0))
                continue
            m = IDENT.match(src, i + 1)
            if m:  # lifetime
                toks.append(("'" + m.group(0), line))
                i = m.end()
                continue
            i += 1
            continue
        m = IDENT.match(src, i)
        if m:
            toks.append((m.group(0), line))
            i = m.end()
            continue
        toks.append((c, line))
        i += 1
    return toks


def match_close(toks, i):
    """toks[i] is '{'; return the index of its matching '}'."""
    depth = 0
    for j in range(i, len(toks)):
        t = toks[j][0]
        if t == '{':
            depth += 1
        elif t == '}':
            depth -= 1
            if depth == 0:
                return j
    return len(toks) - 1


def own_awaits(toks, a, b):
    """Count awaits between a and b, skipping nested async blocks, async closures and fns."""
    count, j = 0, a + 1
    while j < b:
        t = toks[j][0]
        if t == 'await' and toks[j - 1][0] == '.':
            count += 1
        elif t in AWAITING_MACROS and j + 1 < b and toks[j + 1][0] == '!':
            count += 1
        elif t == 'async':
            k = j + 1
            if k < b and toks[k][0] == 'move':
                k += 1
            if k < b and toks[k][0] == '{':
                j = match_close(toks, k) + 1
                continue
            if k < b and toks[k][0] == '|':
                k += 1
                while k < b and toks[k][0] != '|':
                    k += 1
                k += 1
                if k < b and toks[k][0] == '{':
                    j = match_close(toks, k) + 1
                    continue
        elif t == 'fn' and toks[j - 1][0] != '.':
            k = j
            while k < b and toks[k][0] not in '{;':
                k += 1
            if k < b and toks[k][0] == '{':
                j = match_close(toks, k) + 1
                continue
        j += 1
    return count


def is_test_path(rel):
    return '/tests/' in rel or rel.startswith('tests/') or 'test' in os.path.basename(rel)


def scan(path, rel):
    """Yield findings as (shape, kind, trait, rel, line, name, in_test)."""
    with open(path, encoding='utf-8', errors='replace') as f:
        toks = tokenize(f.read())
    test_file = is_test_path(rel)
    stack = []  # (kind, end_index, is_test, trait_name)
    pending_test_attr = False
    i = 0
    while i < len(toks):
        while stack and i > stack[-1][1]:
            stack.pop()
        t = toks[i][0]
        in_test = test_file or any(s[2] for s in stack)

        if t == '#' and i + 1 < len(toks) and toks[i + 1][0] == '[':
            depth, j = 0, i + 1
            while j < len(toks):
                if toks[j][0] == '[':
                    depth += 1
                elif toks[j][0] == ']':
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            attr = [x[0] for x in toks[i + 2:j]]
            if attr[:1] == ['cfg'] and 'test' in attr and 'not' not in attr:
                pending_test_attr = True
            elif attr[:1] != ['cfg'] and 'test' in attr:
                pending_test_attr = True
            i = j + 1
            continue

        if t == 'mod' and i + 2 < len(toks) and toks[i + 2][0] == '{':
            k = i + 2
            stack.append(('mod', match_close(toks, k), pending_test_attr or 'test' in toks[i + 1][0], ''))
            pending_test_attr = False
            i = k + 1
            continue

        if t in ('impl', 'trait') and (i == 0 or toks[i - 1][0] not in ('.', ':', '&', '<', '(', ',')):
            k, angle, has_for, trait_name = i + 1, 0, False, ''
            while k < len(toks) and not (toks[k][0] == '{' and angle == 0) and toks[k][0] != ';':
                x = toks[k][0]
                if x == '<':
                    angle += 1
                elif x == '>' and toks[k - 1][0] != '-':
                    angle -= 1
                elif x == 'for' and angle == 0 and toks[k + 1][0] != '<':
                    has_for = True
                elif angle == 0 and not has_for and IDENT.fullmatch(x):
                    trait_name = x  # last path segment before generics and `for`
                k += 1
            if k < len(toks) and toks[k][0] == '{':
                kind = 'trait' if t == 'trait' else ('trait_impl' if has_for else 'inherent_impl')
                stack.append((kind, match_close(toks, k), pending_test_attr, trait_name if has_for else ''))
                pending_test_attr = False
                i = k + 1
                continue

        if t == 'fn' and toks[i - 1][0] != '.' and i + 1 < len(toks) and IDENT.fullmatch(toks[i + 1][0]):
            name, fline = toks[i + 1][0], toks[i][1]
            k, is_async = i - 1, False
            while k >= 0 and toks[k][0] in ('pub', 'crate', '(', ')', 'super', 'in', 'self',
                                             'unsafe', 'const', 'extern', '"str"', 'async', 'default'):
                if toks[k][0] == 'async':
                    is_async = True
                k -= 1
            k, depth = i, 0
            while k < len(toks):
                x = toks[k][0]
                if x in '([<':
                    depth += 1
                elif x in ')]>' and not (x == '>' and toks[k - 1][0] == '-'):
                    depth -= 1
                elif x in '{;' and depth <= 0:
                    break
                k += 1
            fn_test = pending_test_attr
            pending_test_attr = False
            if k < len(toks) and toks[k][0] == '{':
                end = match_close(toks, k)
                if is_async and own_awaits(toks, k, end) == 0:
                    ctx = next((s for s in reversed(stack) if s[0] not in ('mod', 'fn')), None)
                    kind = ctx[0] if ctx else 'free'
                    yield ('async_fn', kind, ctx[3] if ctx else '', rel, fline, name, in_test or fn_test)
                stack.append(('fn', end, fn_test, ''))
            i = k + 1
            continue

        if t == 'async':
            k = i + 1
            if k < len(toks) and toks[k][0] == 'move':
                k += 1
            if k < len(toks) and toks[k][0] == '{':
                end = match_close(toks, k)
                if own_awaits(toks, k, end) == 0:
                    ctx = next((s for s in reversed(stack) if s[0] == 'trait_impl'), None)
                    yield ('async_block', '', ctx[3] if ctx else '', rel, toks[i][1], '', in_test)
        i += 1


def scan_tree(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in ('target', '.git', 'node_modules'))
        for name in sorted(filenames):
            if name.endswith('.rs'):
                path = os.path.join(dirpath, name)
                yield from scan(path, os.path.relpath(path, root))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    ap.add_argument('root', nargs='?', default='.', help='directory to scan (default: .)')
    ap.add_argument('--tests', action='store_true', help='include test code')
    args = ap.parse_args()

    found = [r for r in scan_tree(args.root) if args.tests or not r[6]]
    fns = [r for r in found if r[0] == 'async_fn']
    blocks = [r for r in found if r[0] == 'async_block']
    scope = 'including tests' if args.tests else 'non-test'

    print(f'== async fn with no .await of its own ({scope}): {len(fns)}')
    by_kind = defaultdict(list)
    for r in fns:
        by_kind[r[1]].append(r)
    labels = {
        'trait_impl': 'in trait impls (clippy unused_async skips these)',
        'trait': 'trait default methods',
        'inherent_impl': 'inherent methods',
        'free': 'free functions',
    }
    for kind, label in labels.items():
        print(f'-- {label}: {len(by_kind[kind])}')
        for _, _, trait, rel, line, name, _ in by_kind[kind]:
            suffix = f' [impl {trait}]' if trait else ''
            print(f'   {rel}:{line} {name}{suffix}')

    print(f'== async blocks with no .await ({scope}): {len(blocks)}')
    per_file = defaultdict(int)
    for _, _, trait, rel, _, _, _ in blocks:
        per_file[(rel, trait)] += 1
    for (rel, trait), count in sorted(per_file.items(), key=lambda x: (-x[1], x[0])):
        suffix = f' [in impl {trait}]' if trait else ''
        print(f'   {count:3d} {rel}{suffix}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
