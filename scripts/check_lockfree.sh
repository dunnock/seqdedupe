#!/usr/bin/env bash
# Verify that the 128-bit atomics compile to single lock-free instructions:
#  * header CAS/loads are inline `lock cmpxchg16b` / `vmovdqa`,
#  * nothing calls a libatomic / compiler-rt helper (which may take a lock),
#  * the register/snapshot hot paths contain no calls other than the cold
#    reclaim path and panics.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release --offline --examples -q 2>/dev/null || cargo build --release -q --examples
bin=target/release/examples/asm_probe
dis=$(mktemp)
objdump -d --no-show-raw-insn -M intel -C "$bin" >"$dis"

fail=0
if grep -Eq 'call.*(__atomic_|__sync_|__cmpxchg|atomic_compare_exchange_16)' "$dis"; then
    echo "FAIL: call to an out-of-line atomic helper:"; grep -E 'call.*(__atomic_|__sync_)' "$dis" | head
    fail=1
fi
n_cas=$(grep -c 'lock cmpxchg16b' "$dis" || true)
n_ld=$(grep -c 'vmovdqa xmm[0-9]*,XMMWORD PTR' "$dis" || true)
echo "lock cmpxchg16b sites: $n_cas"
echo "vmovdqa 16-byte load sites (all, incl. non-header): $n_ld"
[ "$n_cas" -gt 0 ] || { echo "FAIL: no cmpxchg16b emitted"; fail=1; }

# Calls inside register/snapshot (everything below them is inlined).
awk '/<asm_probe::probe_(register|snapshot)>:$/{p=1;next} /^$/{p=0} p' "$dis" >"$dis.reg"
[ -s "$dis.reg" ] || { echo "FAIL: probe functions not found"; exit 1; }
# Resolve direct targets and PIC indirect calls through the GOT
# (`call [rip+x] # <GOT addr>` -> relocation -> symbol).
resolve() {
    while read -r line; do
        if [[ $line =~ call\ +[0-9a-f]+\ \<(.*)\> ]]; then
            echo "${BASH_REMATCH[1]}"
        elif [[ $line =~ \#\ ([0-9a-f]+)\ \< ]]; then
            got=${BASH_REMATCH[1]}
            tgt=$(objdump -R "$bin" | awk -v g="$got" '{a=$1; sub(/^0+/,"",a)} a==g {print $3}' | sed -E 's/.*\+0x0*//')
            nm -C "$bin" | awk -v t="$tgt" '{a=$1; sub(/^0+/,"",a)} a==t {$1="";$2=""; print substr($0,3)}'
        fi
    done
}
echo "calls inside probe_register/probe_snapshot:"
grep -E '\bcall\b' "$dis.reg" | resolve | sort | uniq -c | tee "$dis.calls"
# Allowed: the cold reclaim scan, panics, and RawVec::grow_one from
# `retired.push` (never taken: free + retired + in_hand == preallocated
# capacity at all times).
if grep -vqE 'reclaim|reg_slot_avx|panic|grow_one' "$dis.calls"; then
    echo "FAIL: unexpected call in register/snapshot hot path"; fail=1
fi
rm -f "$dis" "$dis.reg" "$dis.calls"
[ $fail -eq 0 ] && echo "OK: header atomics are inline single instructions (lock-free)."
exit $fail
