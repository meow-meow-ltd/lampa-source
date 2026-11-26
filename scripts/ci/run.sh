#!/usr/bin/env bash
# scripts/ci/run.sh — local CI chain for Lampa P0/W0.
#
# Why local: W0 forbids push/PR, so "CI is green" means this script exits 0
# (docs/decisions.md W0-5).  .github/workflows/ci.yml mirrors the six original
# gates but is not executed in W0 (no remote Actions run, see О11) and has no
# `backup-timing` step (the heavy budget run is local-only).
#
# Steps (each prints one PASS/FAIL/SKIP line; any FAIL makes the exit code 1):
#   1. fmt           cargo fmt --check
#   2. clippy        cargo clippy --all-targets -- -D warnings
#   3. test          cargo test
#   4. backup-timing cargo test --release --bins -- --ignored --test-threads=1
#                    (the two heavy 8 MiB backup/restore timing tests, one at a
#                    time, /tmp on a container-local tmpfs; the PASS/FAIL line
#                    prints the measured ms)
#   5. contract      python3 deploy/scripts/capture_api_golden.py --check  (T03)
#   6. frontend      npm run build (gulp)
#   7. vitest        npx vitest run parsed against the committed W0-4 baseline
#
# The host has no Rust toolchain, so steps 1-4 always run inside
# lampa-rust-builder:1.100.0 with the exact docker recipe from server/README.md.
# Nothing is installed on the host.
#
# Flags:
#   --skip-rust           skip fmt/clippy/test/backup-timing (e.g. while the crate
#                         is being written)
#   --skip-frontend       skip the frontend build AND the vitest baseline (both JS)
#   --skip-contract       skip the golden contract check
#   --strict              missing T03 contract artifacts are a FAIL (G1 run);
#                         without it the contract step auto-SKIPs when T03 is absent
#   --list                list the seven steps and exit 0
#   --baseline-self-test  prove the vitest baseline guard rejects an injected
#                         extra failure and exit 0; does not run vitest
#   -h, --help            show this help
#
# Environment overrides:
#   LAMPA_RUST_BUILDER_IMAGE  default lampa-rust-builder:1.100.0
#   LAMPA_CARGO_CACHE         host dir mounted at /cargo   (default /opt/lampa/poc/cargo-cache)
#   LAMPA_CARGO_TARGET        host dir mounted at /target  (default /opt/lampa/.cargo-target)
#   LAMPA_CONTRACT_SCRIPT     repo-relative T03 script (default deploy/scripts/capture_api_golden.py)
#
# Exit codes: 0 = all requested steps PASS/SKIP, 1 = at least one FAIL,
#             2 = usage error.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GUARD="$ROOT/scripts/ci/vitest_baseline_guard.py"
BASELINE_REL="scripts/ci/baselines/vitest-baseline.json"

BUILDER_IMAGE="${LAMPA_RUST_BUILDER_IMAGE:-lampa-rust-builder:1.100.0}"
CARGO_CACHE_HOST="${LAMPA_CARGO_CACHE:-/opt/lampa/poc/cargo-cache}"
CARGO_TARGET_HOST="${LAMPA_CARGO_TARGET:-/opt/lampa/.cargo-target}"
CONTRACT_SCRIPT="${LAMPA_CONTRACT_SCRIPT:-deploy/scripts/capture_api_golden.py}"

SKIP_RUST=0
SKIP_FRONTEND=0
SKIP_CONTRACT=0
STRICT=0
MODE="run"

PASS_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0
FAILED_STEPS=()

pass() { PASS_COUNT=$((PASS_COUNT + 1)); printf 'PASS %s: %s\n' "$1" "$2"; }
fail() { FAIL_COUNT=$((FAIL_COUNT + 1)); FAILED_STEPS+=("$1"); printf 'FAIL %s: %s\n' "$1" "$2"; }
skip() { SKIP_COUNT=$((SKIP_COUNT + 1)); printf 'SKIP %s: %s\n' "$1" "$2"; }

usage() { awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"; }

list_steps() {
    cat <<'EOF'
Local CI steps (in order):
  1. fmt           cargo fmt --check                                   [--skip-rust skips]
  2. clippy        cargo clippy --all-targets -- -D warnings           [--skip-rust skips]
  3. test          cargo test                                          [--skip-rust skips]
  4. backup-timing cargo test --release --bins -- --ignored --test-threads=1
                   (heavy 8 MiB backup ≤200 ms / restore ≤300 ms; serialized; tmpfs /tmp) [--skip-rust skips]
  5. contract      python3 deploy/scripts/capture_api_golden.py --check [auto-SKIP if T03 absent; --strict fails]
  6. frontend      npm run build (gulp)                                 [--skip-frontend skips]
  7. vitest        npx vitest run vs scripts/ci/baselines/vitest-baseline.json [--skip-frontend skips]

Cargo steps run in the lampa-rust-builder:1.100.0 container (server/README.md recipe).
Default: all seven steps, contract auto-detects T03 golden fixtures.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --skip-rust) SKIP_RUST=1 ;;
        --skip-frontend) SKIP_FRONTEND=1 ;;
        --skip-contract) SKIP_CONTRACT=1 ;;
        --strict) STRICT=1 ;;
        --list) MODE="list" ;;
        --baseline-self-test) MODE="baseline-self-test" ;;
        -h|--help) usage; exit 0 ;;
        *)
            printf 'run.sh: unknown argument: %s (try --help)\n' "$1" >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$MODE" = "list" ]; then
    list_steps
    exit 0
fi

if [ "$MODE" = "baseline-self-test" ]; then
    if ! command -v python3 >/dev/null 2>&1; then
        fail "baseline-self-test" "python3 not found"
        printf '\nfailed steps: %s\n' "${FAILED_STEPS[*]}"
        exit 1
    fi
    ( cd "$ROOT" && python3 "$GUARD" --self-test )
    rc=$?
    if [ "$rc" -eq 0 ]; then
        pass "baseline-self-test" "injected new failing file rejected; baseline guard self-test OK"
        exit 0
    fi
    fail "baseline-self-test" "guard self-test failed (exit $rc)"
    printf '\nfailed steps: %s\n' "${FAILED_STEPS[*]}"
    exit 1
fi

printf '==> local CI: %s\n' "$ROOT"
if [ "$STRICT" -eq 1 ]; then
    printf '==> mode: default steps, contract absence is FATAL (--strict)\n\n'
else
    printf '==> mode: default steps, contract auto-detects T03 artifacts\n\n'
fi

run_in_container() {
    # $1 = repo-relative working dir, $2 = shell command, rest = docker args
    local workdir="$1" command="$2"
    shift 2
    docker run --rm --user "$(id -u):$(id -g)" \
        -v "$ROOT":/work \
        -v "$CARGO_CACHE_HOST":/cargo \
        -v "$CARGO_TARGET_HOST":/target \
        -e CARGO_HOME=/cargo \
        -e CARGO_TARGET_DIR=/target \
        -w "/work/$workdir" \
        "$@" "$BUILDER_IMAGE" \
        bash -c "$command"
}

rust_step() {
    local step="$1" command="$2"
    if [ "$SKIP_RUST" -eq 1 ]; then
        skip "$step" "skipped by --skip-rust (container recipe not run)"
        return
    fi
    if ! command -v docker >/dev/null 2>&1; then
        fail "$step" "docker not found; Cargo steps require the builder container (server/README.md)"
        return
    fi
    if ! docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
        fail "$step" "docker image $BUILDER_IMAGE not found; build it with the one-time recipe in server/README.md"
        return
    fi
    mkdir -p "$CARGO_CACHE_HOST" "$CARGO_TARGET_HOST" 2>/dev/null || true

    local started elapsed rc
    started=$SECONDS
    run_in_container server "$command"
    rc=$?
    elapsed=$((SECONDS - started))
    if [ "$rc" -eq 0 ]; then
        pass "$step" "$command (lampa-rust-builder:1.100.0, ${elapsed}s)"
    else
        fail "$step" "$command exited $rc (lampa-rust-builder:1.100.0, ${elapsed}s)"
    fi
}

# Heavy 8 MiB backup/restore budget tests. They are `#[ignore]`d in
# server/src/db/backup.rs: inside the default parallel `cargo test` the timing
# competes with other test binaries and produces false >200 ms failures
# (G2-02). This step runs only the ignored tests, in release, with
# `--test-threads=1`, and reports the measured ms from --nocapture output.
# The container gets a private tmpfs /tmp (`--tmpfs`) because this host shares
# its disk with other containers/agents: overlay writeback injected 200-900 ms
# spikes into the 8 MiB artefact fsync (measured, G2-02 repair). The tmpfs only
# isolates the measurement environment; the test still creates a real 8 MiB v7
# DB, runs the SQLite backup API, checks 0600/0700 + integrity/quick_check +
# SHA-256, and asserts the unchanged 200/300 ms budgets.
backup_timing_step() {
    local step="backup-timing"
    if [ "$SKIP_RUST" -eq 1 ]; then
        skip "$step" "skipped by --skip-rust (heavy backup/restore timing not run)"
        return
    fi
    if ! command -v docker >/dev/null 2>&1; then
        fail "$step" "docker not found; the timing step requires the builder container (server/README.md)"
        return
    fi
    if ! docker image inspect "$BUILDER_IMAGE" >/dev/null 2>&1; then
        fail "$step" "docker image $BUILDER_IMAGE not found; build it with the one-time recipe in server/README.md"
        return
    fi
    mkdir -p "$CARGO_CACHE_HOST" "$CARGO_TARGET_HOST" 2>/dev/null || true

    local command="cargo test --release --bins -- --ignored --test-threads=1 --nocapture"  # /tmp = tmpfs (see above)
    local log started elapsed rc backup_ms restore_ms heavy
    log="$(mktemp)"
    started=$SECONDS
    run_in_container server "$command" --tmpfs /tmp:rw,size=256m 2>&1 | tee "$log"
    rc=${PIPESTATUS[0]}
    elapsed=$((SECONDS - started))
    # The tests print `backup: N ms (≤200 ms)` (three samples) and
    # `restore: N ms (≤300 ms)`; the first line is prefixed by the test name,
    # so match the fragment anywhere on the line. The PASS/FAIL line reports
    # all samples.
    backup_ms="$(grep -oE 'backup: [0-9]+ ms' "$log" | sed -E 's/backup: ([0-9]+) ms/\1/' | paste -sd/ -)"
    restore_ms="$(grep -oE 'restore: [0-9]+ ms' "$log" | sed -E 's/restore: ([0-9]+) ms/\1/' | paste -sd/ -)"
    heavy="$(sed -nE 's/^test result: ok\. ([0-9]+) passed.*/\1/p' "$log" | head -n 1)"
    rm -f "$log"
    if [ "$rc" -eq 0 ]; then
        pass "$step" "$command (${heavy:-?} heavy tests; backup ${backup_ms:-n/a} ms ≤200, restore ${restore_ms:-n/a} ms ≤300; ${elapsed}s)"
    else
        fail "$step" "$command exited $rc (backup ${backup_ms:-n/a} ms, restore ${restore_ms:-n/a} ms; ${elapsed}s)"
    fi
}

contract_step() {
    local step="contract"
    if [ "$SKIP_CONTRACT" -eq 1 ]; then
        skip "$step" "skipped by --skip-contract"
        return
    fi
    local script="$ROOT/$CONTRACT_SCRIPT"
    if [ ! -f "$script" ]; then
        if [ "$STRICT" -eq 1 ]; then
            fail "$step" "T03 artifact $CONTRACT_SCRIPT is absent and --strict is set"
        else
            skip "$step" "T03 artifact $CONTRACT_SCRIPT absent (auto-detect; --strict turns this into FAIL)"
        fi
        return
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        fail "$step" "python3 not found"
        return
    fi

    local started elapsed rc
    started=$SECONDS
    ( cd "$ROOT" && python3 "$CONTRACT_SCRIPT" --check )
    rc=$?
    elapsed=$((SECONDS - started))
    if [ "$rc" -eq 0 ]; then
        pass "$step" "python3 $CONTRACT_SCRIPT --check (${elapsed}s)"
    else
        fail "$step" "python3 $CONTRACT_SCRIPT --check exited $rc (${elapsed}s)"
    fi
}

is_build_generated_path() {
    # Contract (T05 revision 2): run.sh may restore ONLY public/css/** and
    # plugins/*/css/** — the paths this repo's gulp/sass build writes.  No other
    # tracked path is ever restored, and the script never invokes git's
    # restore/checkout/reset subcommands at all (restore is a read-only
    # `git show HEAD:<path>` + file write).  Everything else may belong to a
    # concurrent writer sharing the workspace, so it is left intact.
    # In a bash `case` pattern `*` also matches `/`, so this is recursive:
    # public/css/* covers public/css/** and plugins/*/css/* covers plugins/*/css/**.
    case "$1" in
        public/css/*|plugins/*/css/*) return 0 ;;
        *) return 1 ;;
    esac
}

restore_build_generated_files() {
    # The upstream gulp/sass chain recompiles a few TRACKED assets in place
    # (public/css/app.css, plugins/*/css/style.css) with newer sass/autoprefixer
    # versions.  That is normal build output, not a source change: restore the
    # files that were clean before the build so `git status` still shows only
    # the CI-skeleton additions.  Files that were already modified/staged before
    # the build are never touched, and files outside the frontend output paths
    # are never touched even if they change while the build runs (concurrent
    # writers share this workspace).
    local before="$1" after="$2" restored=0 path
    local new_paths
    new_paths="$(comm -13 <(LC_ALL=C sort "$before") <(LC_ALL=C sort "$after"))"
    [ -n "$new_paths" ] || { printf '0'; return; }

    local staged
    staged="$(git -C "$ROOT" diff --cached --name-only 2>/dev/null || true)"
    while IFS= read -r path; do
        [ -n "$path" ] || continue
        if ! is_build_generated_path "$path"; then
            printf 'run.sh: NOTE: %s changed during the frontend build; not restoring (outside public/css or plugins/*/css)\n' "$path" >&2
            continue
        fi
        if printf '%s\n' "$staged" | grep -Fxq -- "$path"; then
            printf 'run.sh: WARN: build modified staged file %s; not restoring\n' "$path" >&2
            continue
        fi
        if git -C "$ROOT" show "HEAD:$path" > "$ROOT/$path" 2>/dev/null; then
            restored=$((restored + 1))
            printf 'run.sh: restored build-generated tracked file: %s\n' "$path" >&2
        fi
    done <<< "$new_paths"
    printf '%s' "$restored"
}

frontend_step() {
    local step="frontend"
    if [ "$SKIP_FRONTEND" -eq 1 ]; then
        skip "$step" "skipped by --skip-frontend"
        return
    fi
    if ! command -v npm >/dev/null 2>&1; then
        fail "$step" "npm not found"
        return
    fi
    if [ ! -d "$ROOT/node_modules" ]; then
        fail "$step" "node_modules missing; run npm ci first"
        return
    fi

    local before after started elapsed rc restored note
    before="$(mktemp)"
    after="$(mktemp)"
    git -C "$ROOT" diff --name-only > "$before" 2>/dev/null || : > "$before"

    started=$SECONDS
    ( cd "$ROOT" && npm run build )
    rc=$?
    elapsed=$((SECONDS - started))

    git -C "$ROOT" diff --name-only > "$after" 2>/dev/null || : > "$after"
    restored="$(restore_build_generated_files "$before" "$after")"
    rm -f "$before" "$after"
    note=""
    if [ "${restored:-0}" -gt 0 ]; then
        note=", restored $restored build-generated tracked file(s)"
    fi
    if [ "$rc" -eq 0 ]; then
        pass "$step" "npm run build (${elapsed}s$note)"
    else
        fail "$step" "npm run build exited $rc (${elapsed}s$note)"
    fi
}

vitest_step() {
    local step="vitest"
    if [ "$SKIP_FRONTEND" -eq 1 ]; then
        skip "$step" "skipped by --skip-frontend (JS toolchain)"
        return
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        fail "$step" "python3 not found"
        return
    fi
    if [ ! -d "$ROOT/node_modules" ]; then
        fail "$step" "node_modules missing; run npm ci first"
        return
    fi

    local started elapsed rc
    started=$SECONDS
    ( cd "$ROOT" && python3 "$GUARD" --run )
    rc=$?
    elapsed=$((SECONDS - started))
    case "$rc" in
        0) pass "$step" "equal-or-better than $BASELINE_REL (4 failed files / 3 failed tests / 171 passed baseline; ${elapsed}s)" ;;
        1) fail "$step" "NEW failures vs $BASELINE_REL (${elapsed}s)" ;;
        *) fail "$step" "baseline guard error (exit $rc, ${elapsed}s)" ;;
    esac
}

rust_step "fmt" "cargo fmt --check"
rust_step "clippy" "cargo clippy --all-targets -- -D warnings"
rust_step "test" "cargo test"
backup_timing_step
contract_step
frontend_step
vitest_step

printf '\nci summary: %d passed, %d failed, %d skipped\n' "$PASS_COUNT" "$FAIL_COUNT" "$SKIP_COUNT"
if [ "$FAIL_COUNT" -gt 0 ]; then
    printf 'failed steps: %s\n' "${FAILED_STEPS[*]}"
    exit 1
fi
if [ "$SKIP_COUNT" -gt 0 ]; then
    printf 'ci OK (with skips; use --strict for the G1 contract gate)\n'
else
    printf 'ci OK\n'
fi
exit 0
