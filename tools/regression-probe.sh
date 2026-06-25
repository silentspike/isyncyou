#!/usr/bin/env bash
# W0.1 regression probes (#85, AC3 + AC4): structural DOM + mobile-metric checks
# that freeze the landed security/UX audit fixes (#73 iframe sandbox, #74 OneNote
# no-body empty state, #75 overlay-close-on-route-change). Screenshots are evidence,
# this is the gate. Drives the running daemon's web UI through playwright-cli, using
# the app's own go()/DOM clicks so it survives ref churn.
#
# Usage:  tools/regression-probe.sh [URL]      (default http://127.0.0.1:8869/)
# Exits non-zero on the first failed assertion.
set -uo pipefail
URL="${1:-http://127.0.0.1:8869/}"
S="rprobe$$"
PASS=0; FAIL=0

ev() { playwright-cli -s="$S" eval "$1" 2>&1 | grep -A1 '^### Result' | tail -1; }
nav() { ev "() => { window.go('$1'); return 1; }" >/dev/null; sleep 2; }
# check <name> <actual> <expected-substring>
check() {
  if [[ "$2" == *"$3"* ]]; then echo "  PASS  $1  → $2"; PASS=$((PASS+1));
  else echo "  FAIL  $1  → got [$2] want [*$3*]"; FAIL=$((FAIL+1)); fi
}

playwright-cli -s="$S" open "$URL" >/dev/null 2>&1
playwright-cli -s="$S" resize 1280 900 >/dev/null 2>&1
sleep 2

echo "== AC3: security/UX DOM invariants (desktop) =="

# AC3a — the mail body renders in a sandboxed iframe (#73)
nav mail
ev "() => { const r=document.querySelector('.mail-item'); if(r) r.click(); return 1; }" >/dev/null; sleep 2
check "mail body iframe is sandboxed" "$(ev "() => { const f=document.querySelector('.mail-frame'); return f? f.getAttribute('sandbox') : 'NO-IFRAME'; }")" "allow-same-origin"

# AC3c — a OneNote page with no archived body shows a native empty state, never a
# 404/JSON iframe (#74)
nav onenote
ev "() => { const ls=[...document.querySelectorAll('.note-leaf')]; const t=ls.find(x=>/untitled/i.test(x.textContent))||ls[ls.length-1]; if(t)t.click(); return 1; }" >/dev/null; sleep 2
check "onenote no-body → empty state, no iframe" "$(ev "() => { const r=document.querySelector('.note-reader'); return 'iframe='+(!!r.querySelector('iframe'))+',empty='+(!!r.querySelector('.empty,.note-empty,[class*=empty]')); }")" "iframe=false,empty=true"

# AC3b — opening an overlay then changing route removes every .scrim/.sheet (#75)
nav calendar
ev "() => { const b=[...document.querySelectorAll('button')].find(x=>/\d\d:\d\d/.test(x.textContent)&&x.closest('[class*=cal]')); if(b)b.click(); return 1; }" >/dev/null; sleep 1
OPENED="$(ev "() => document.querySelectorAll('.scrim,.sheet').length")"
nav mail
check "overlay opened then closed on route change (was $OPENED)" "$(ev "() => document.querySelectorAll('.scrim,.sheet').length")" "0"

echo "== AC4: mobile metrics — no horizontal overflow + touch targets (390x844) =="
playwright-cli -s="$S" resize 390 844 >/dev/null 2>&1
sleep 1
for V in overview mail onedrive calendar contacts todo onenote; do
  nav "$V"
  check "$V: no horizontal overflow" "$(ev "() => String(document.documentElement.scrollWidth <= document.documentElement.clientWidth)")" "true"
done
# touch targets: the smallest visible primary/ghost button across the app must stay
# tappable (>= 28px tall). Reports the measured minimum.
nav mail
MINBTN="$(ev "() => { const hs=[...document.querySelectorAll('.btn, .nav-subitem, .seg-btn')].map(e=>e.getBoundingClientRect().height).filter(h=>h>0); return hs.length? Math.round(Math.min(...hs)) : -1; }")"
check "min interactive target height >= 28px (measured ${MINBTN}px)" "$([[ "$MINBTN" -ge 28 ]] && echo OK)" "OK"

playwright-cli -s="$S" close >/dev/null 2>&1
echo "== result: $PASS passed, $FAIL failed =="
[[ "$FAIL" -eq 0 ]]
