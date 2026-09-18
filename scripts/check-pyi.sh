#!/usr/bin/env bash
# check-pyi.sh — the stub-docstring drift check.
#
# `bindings/python/mushroomdb.pyi` carries the full signature of every method
# the PyO3 module exposes. The signature is enforced already: `tests/parity.py`
# fails when a public method is missing from the stub. The *docstring* was not,
# and drifted: a method whose Rust doc comment documented an argument's whole
# contract sat in the stub under a one-line summary that named neither. An IDE
# hover and a type checker read the stub, not the compiled `__doc__`, so the
# thinner text is the one a caller actually sees.
#
# The rule: a stub docstring may not be a single line while the Rust doc
# comment it stands for runs to more than three. Below that threshold the
# one-liner is the whole contract and repeating it buys nothing.
#
# Doc lines are counted as non-empty content lines — a bare `///` and a blank
# line inside a docstring count for neither side — so reflowing prose does not
# move the count.
#
# Exits 1 printing every drifted method with both counts. Run by CI alongside
# scripts/check-claims.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RUST="bindings/python/src/lib.rs"
STUB="bindings/python/mushroomdb.pyi"

for f in "$RUST" "$STUB"; do
  if [[ ! -f "$f" ]]; then
    echo "check-pyi.sh: missing $f" >&2
    exit 1
  fi
done

# Methods whose stub docstring is deliberately shorter than the Rust comment.
# A name here needs a reason beside it; "it is long" is not one.
#
#   __enter__/__exit__  — the stub declares them with `...` and no docstring at
#                         all, which is what a type checker wants for protocol
#                         methods.
EXEMPT="__enter__ __exit__"

# ── Rust: doc-comment length per `#[pymethods]` fn ────────────────────────────
#
# Doc lines accumulate until a `fn` line consumes them. Attribute lines
# (`#[staticmethod]`, `#[pyo3(...)]`) sit between the comment and the fn and
# must not clear the buffer. `#[pyo3(signature = ...)]` wraps across lines, so
# the attribute is skipped by bracket depth rather than by line. Anything else
# clears the buffer, so a doc comment on a struct or a `use` never gets
# attributed to the next fn down.
rust_counts() {
  awk '
    inattr                  { d = $0
                              nopen = gsub(/\[/, "[", d)
                              nshut = gsub(/\]/, "]", d)
                              attr += nopen - nshut
                              if (attr <= 0) inattr = 0
                              next }
    /^[[:space:]]*\/\/\//   { line = $0
                              sub(/^[[:space:]]*\/\/\/[[:space:]]?/, "", line)
                              if (line !~ /^[[:space:]]*$/) n++
                              next }
    /^[[:space:]]*#\[/      { d = $0
                              nopen = gsub(/\[/, "[", d)
                              nshut = gsub(/\]/, "]", d)
                              attr = nopen - nshut
                              if (attr > 0) inattr = 1
                              next }
    /^[[:space:]]*(pub )?fn [A-Za-z_]/ {
                              if (n > 0) {
                                name = $0
                                sub(/^[[:space:]]*(pub )?fn /, "", name)
                                sub(/[(<].*$/, "", name)
                                print name, n
                              }
                              n = 0; next }
    /^[[:space:]]*$/        { next }
                            { n = 0 }
  ' "$RUST"
}

# ── Stub: docstring length per `def` ─────────────────────────────────────────
#
# A signature may span many lines, so the docstring is whatever opens on the
# first line after the one closing the parameter list. `"""x"""` on one line is
# one line; otherwise content lines are counted until the closing `"""`.
stub_counts() {
  awk '
    /^    def [A-Za-z_]/ {
        name = $0
        sub(/^    def /, "", name)
        sub(/\(.*$/, "", name)
        indef = 1; depth = 0
    }
    indef {
        # Track parenthesis depth across the signature to find where it ends.
        line = $0
        nopen = gsub(/\(/, "(", line)
        nshut = gsub(/\)/, ")", line)
        depth += nopen - nshut
        if (depth <= 0) { indef = 0; want_doc = 1 }
        next
    }
    want_doc {
        if ($0 ~ /^[[:space:]]*$/) next
        if ($0 !~ /"""/) { print name, 0; want_doc = 0; next }
        # Single-line docstring: opening and closing quotes on the same line.
        body = $0
        sub(/^[[:space:]]*"""/, "", body)
        if (body ~ /"""/) { print name, 1; want_doc = 0; next }
        n = (body ~ /^[[:space:]]*$/) ? 0 : 1
        want_doc = 0; indoc = 1; next
    }
    indoc {
        if ($0 ~ /"""/) {
            body = $0
            sub(/""".*$/, "", body)
            if (body !~ /^[[:space:]]*$/) n++
            print name, n
            indoc = 0; n = 0; next
        }
        if ($0 !~ /^[[:space:]]*$/) n++
    }
  ' "$STUB"
}

RUST_TABLE="$(rust_counts)"
STUB_TABLE="$(stub_counts)"

fail=0
while read -r name stub_n; do
  [[ -n "$name" ]] || continue
  case " $EXEMPT " in *" $name "*) continue ;; esac
  [[ "$stub_n" -le 1 ]] || continue
  rust_n="$(printf '%s\n' "$RUST_TABLE" | awk -v n="$name" '$1 == n { print $2; exit }')"
  [[ -n "$rust_n" ]] || continue
  if [[ "$rust_n" -gt 3 ]]; then
    echo "$STUB: $name — stub docstring is $stub_n line(s); $RUST documents it in $rust_n" >&2
    fail=1
  fi
done <<< "$STUB_TABLE"

if [[ "$fail" -ne 0 ]]; then
  echo "check-pyi.sh: FAILED — the stub says less than the binding it stands for" >&2
  exit 1
fi
echo "check-pyi.sh: OK"
