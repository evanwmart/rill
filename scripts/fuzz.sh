#!/usr/bin/env bash
# Run every fuzz target for a bounded time and report.
#
# Fuzzing was manual — "cargo +nightly fuzz run <target>", one target, for
# however long you remembered to leave it. That is fine for chasing a
# specific parser and useless as a routine: the targets that matter are the
# ones nobody thought to re-run after changing the format next door.
#
#   scripts/fuzz.sh                 # 60s per target, the pre-push-ish sweep
#   scripts/fuzz.sh 900             # 15 min per target, the overnight-ish one
#   scripts/fuzz.sh 300 doc_decode  # one target, longer
#   scripts/fuzz.sh --minimize      # shrink the committed corpora
#   scripts/fuzz.sh --audit         # fetch advisories and check the lock
#
# Every run grows the corpus, so run --minimize before committing one: it
# keeps the smallest set of inputs preserving coverage (11M/2548 files ->
# 5.2M/1238 on its first outing). It renames survivors to content hashes,
# which discards the readable seed names — that is fine, because the seeds
# are regenerated from the `write_fuzz_corpus` tests in rill-doc, rill-pack
# and rill-ui. The corpus is derived and disposable; those tests are the
# source of truth.
#
# Findings land in fuzz/artifacts/<target>/ and the corpus grows in place;
# both are worth committing when a run finds something. Exit status is
# non-zero if any target failed, so this is usable from cron:
#
#   0 3 * * *  cd /path/to/rill && scripts/fuzz.sh 1800 >> /tmp/rill-fuzz.log 2>&1
#   0 8 * * *  cd /path/to/rill && scripts/fuzz.sh --audit >> /tmp/rill-audit.log 2>&1
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

# --- dependency advisories -------------------------------------------------
# The pre-push hook checks the *cached* advisory DB so a push works offline.
# This is the run that fetches, so the cron slot below is what actually
# notices an advisory published since the last push. Un-actionable advisories
# are listed with reasons in .cargo/audit.toml, so anything printed here is
# new and wants triage — fix it, or add it there with the reason why not.
if [ "${1:-}" = "--audit" ]; then
    command -v cargo-audit >/dev/null || {
        echo "cargo-audit not installed: cargo install cargo-audit --locked" >&2
        exit 2
    }
    echo "▶ advisories — $(date -Is)"
    cargo audit --deny warnings || {
        echo "NEW ADVISORY — see docs/dependency-audit.md for the triage pattern" >&2
        exit 1
    }
    echo "✓ no new advisories"
    exit 0
fi

minimize=false
seconds=60
if [ "${1:-}" = "--minimize" ]; then
    minimize=true
    shift
else
    # A leading number is the per-target budget; anything else is a target.
    if [[ "${1:-}" =~ ^[0-9]+$ ]]; then
        seconds="$1"
        shift
    fi
fi
targets=("$@")
if [ ${#targets[@]} -eq 0 ]; then
    # Every [[bin]] in the fuzz crate, so a new target is swept the day it
    # lands rather than the day someone remembers to add it here.
    mapfile -t targets < <(sed -n 's/^name = "\(.*\)"$/\1/p' fuzz/Cargo.toml | tail -n +2)
fi

if ! command -v cargo-fuzz >/dev/null; then
    echo "cargo-fuzz not installed: cargo install cargo-fuzz" >&2
    exit 2
fi
if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
    echo "fuzzing needs nightly: rustup toolchain install nightly" >&2
    exit 2
fi

# alsa-sys needs the libasound shim on this machine (see docs/ and
# scripts/demo-desktop.sh); harmless when the real package is installed.
libshim="$HOME/.cache/rill-libshim"
if [ -f "$libshim/pkgconfig/alsa.pc" ]; then
    export PKG_CONFIG_PATH="$libshim/pkgconfig:${PKG_CONFIG_PATH:-}"
fi

if $minimize; then
    echo "minimizing ${#targets[@]} corpus/corpora — $(date -Is)"
else
    echo "fuzzing ${#targets[@]} target(s) for ${seconds}s each — $(date -Is)"
fi
failed=()
for target in "${targets[@]}"; do
    printf '\n=== %s\n' "$target"
    before=$(ls "fuzz/corpus/$target" 2>/dev/null | wc -l)
    if $minimize; then
        cargo +nightly fuzz cmin "$target" 2>&1 | tail -1
        status=${PIPESTATUS[0]}
    else
        # -max_total_time bounds the run; libFuzzer still exits non-zero the
        # moment it finds a crash, which is the signal we care about.
        cargo +nightly fuzz run "$target" -- \
            -max_total_time="$seconds" -print_final_stats=1 2>&1 |
            grep -E "stat::number_of_executed_units|stat::average_exec_per_sec|stat::new_units_added|ERROR|panicked at|Done .* runs"
        status=${PIPESTATUS[0]}
    fi
    after=$(ls "fuzz/corpus/$target" 2>/dev/null | wc -l)
    echo "corpus: $before -> $after"
    if [ "$status" -ne 0 ]; then
        failed+=("$target")
        echo "FAILED: $target — see fuzz/artifacts/$target/" >&2
    fi
done

if $minimize; then
    printf '\nregenerating the named seed inputs the minimizer renamed away:\n'
    for pkg in "rill-doc --test doc" "rill-pack" "rill-ui"; do
        # shellcheck disable=SC2086
        cargo test -p $pkg -- --ignored write_fuzz >/dev/null 2>&1 &&
            echo "  seeded from $pkg" || echo "  no seeder in $pkg" >&2
    done
fi

printf '\n'
if [ ${#failed[@]} -gt 0 ]; then
    echo "failing targets: ${failed[*]}" >&2
    echo "reproduce with: cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<crash-file>" >&2
    exit 1
fi
echo "all targets clean — $(date -Is)"
