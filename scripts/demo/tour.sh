#!/usr/bin/env bash
#
# The homepage's five-minute tour, run for real, as a script (Z5.3).
#
# It reads the tour's commands from docs/website/index.html (the elements
# marked data-tour), types each one with a short human-like delay, runs it and
# shows its output. Where the tour tells a person to wait, it waits by polling
# the cluster's real state, then says how long the wait really took, so the
# recording can trim idle time without hiding it.
#
# Usage:
#   scripts/demo/tour.sh --check
#       Check that the script knows how to run every command on the homepage,
#       then exit. tests/suite/website.rs runs this.
#   scripts/demo/tour.sh [--setup DEV_BINARIES_DIR]
#       Run the tour. Without --setup it needs a running quickstart cluster
#       and starts at `relish apply`. With --setup it first builds one with
#       `relish setup --quickstart --development-binaries DEV_BINARIES_DIR`,
#       which is what the install line runs once the release is published.
#   scripts/demo/tour.sh --record CAST [--setup DEV_BINARIES_DIR]
#       Record the run with asciinema into CAST (110x32, idle time cut to
#       2 s), then play the setup step SETUP_SPEEDUP (4) times faster. Setup
#       redraws its timers several times a second, so idle trimming alone
#       would leave a minute and a half of VM boots. Both are said on screen.
#
# Environment:
#   RELISH   the relish binary to run (default: the one on PATH)
#
# The script needs bash 3.2 (macOS's /bin/bash), so no associative arrays.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
PAGE="${REPO_DIR}/docs/website/index.html"
DEMO_URL="https://reliaburger.com/demo/podinfo.yaml"
DEMO_FILE="examples/kubernetes/podinfo.yaml"
INGRESS="http://podinfo.localhost:18080"

IDLE_LIMIT=2
SETUP_SPEEDUP=4

MODE="run"
SETUP_BINARIES=""
CAST=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --check) MODE="check"; shift ;;
        --setup) SETUP_BINARIES="${2:?--setup needs a directory}"; shift 2 ;;
        --record) MODE="record"; CAST="${2:?--record needs a file}"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 64 ;;
    esac
done
# Set when this script runs inside its own recording, so the narration only
# mentions trimming and speed-ups that really happen.
RECORDING="${TOUR_RECORDING:-}"

# The text of every <code data-tour> element, one per line, entities decoded.
tour_commands() {
    grep -o '<code data-tour>[^<]*</code>' "${PAGE}" \
        | sed -e 's/<code data-tour>//' -e 's/<\/code>//' \
              -e 's/&lt;/</g' -e 's/&gt;/>/g' -e 's/&quot;/"/g' -e "s/&#39;/'/g" -e 's/&amp;/\&/g'
}

# Every command the tour shows must be one this script knows how to run and
# wait for. A new or changed command fails here, and so in CI, until the
# script learns it.
known_command() {
    case "$1" in
        "curl -fsSL https://reliaburger.com/install.sh | sh" \
        | "relish apply -f ${DEMO_URL}" \
        | "relish status" \
        | "relish path frontend --to redis" \
        | "relish metrics frontend" \
        | "relish fault delay redis 300ms --from frontend --duration 2m --acknowledge" \
        | "relish path frontend --to redis --count 3" \
        | "relish metrics frontend --name http_request_duration_seconds" \
        | "relish dashboard" \
        | "relish fault kill frontend --count 1 --acknowledge" \
        | "relish local stop node-3" \
        | "relish wtf" \
        | "relish local destroy --yes" \
        | "relish uninstall" \
        | "relish manual tour") return 0 ;;
        *) return 1 ;;
    esac
}

if [[ "${MODE}" == "check" ]]; then
    count=0
    unknown=0
    while IFS= read -r command; do
        count=$((count + 1))
        if ! known_command "${command}"; then
            echo "scripts/demo/tour.sh doesn't know how to run: ${command}" >&2
            unknown=1
        fi
    done < <(tour_commands)
    if [[ "${count}" -lt 10 ]]; then
        echo "found only ${count} tour commands in ${PAGE}; did the markup change?" >&2
        exit 1
    fi
    exit "${unknown}"
fi

if [[ "${MODE}" == "record" ]]; then
    command -v asciinema >/dev/null || { echo "asciinema isn't installed" >&2; exit 1; }
    inner="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
    if [[ -n "${SETUP_BINARIES}" ]]; then
        inner="${inner} --setup ${SETUP_BINARIES}"
    fi
    TOUR_RECORDING=1 asciinema rec --headless --overwrite --return \
        --window-size 110x32 --idle-time-limit "${IDLE_LIMIT}" \
        --title "Reliaburger: the five-minute tour" -c "${inner}" "${CAST}"
    # Play setup faster: divide the gaps between its first and last lines.
    # asciicast v3 stores each event's gap since the previous one.
    python3 - "${CAST}" "${SETUP_SPEEDUP}" <<'PYTHON'
import json, sys
path, factor = sys.argv[1], float(sys.argv[2])
lines = open(path, encoding="utf-8").read().splitlines()
header, events = json.loads(lines[0]), [json.loads(line) for line in lines[1:]]
# The recorder's absolute path and shell say nothing about the tour.
header.pop("command", None)
header.pop("env", None)
start = next((i for i, e in enumerate(events) if "faster than it ran" in str(e[2])), None)
if start is not None:
    end = next(i for i, e in enumerate(events[start:], start) if "ready in" in str(e[2]))
    for event in events[start + 1 : end + 1]:
        event[0] = round(event[0] / factor, 3)
with open(path, "w", encoding="utf-8") as cast:
    cast.write(json.dumps(header) + "\n")
    for event in events:
        cast.write(json.dumps(event, ensure_ascii=False) + "\n")
PYTHON
    exit 0
fi

cd "${REPO_DIR}"
if [[ -n "${RELISH:-}" ]]; then
    PATH="$(cd "$(dirname "${RELISH}")" && pwd):${PATH}"
fi
command -v relish >/dev/null || { echo "relish isn't on PATH; set RELISH" >&2; exit 1; }

BOLD=$'\033[1m'
DIM=$'\033[2m'
GREEN=$'\033[32m'
RESET=$'\033[0m'

# A comment line, the way a person would narrate at a shell prompt.
say() {
    printf '%s# %s%s\n' "${DIM}" "$*" "${RESET}"
}

# Type a command at a prompt, a character at a time.
type_command() {
    local text="$1" i
    printf '%s$%s ' "${GREEN}${BOLD}" "${RESET}"
    sleep 0.6
    for ((i = 0; i < ${#text}; i++)); do
        printf '%s' "${text:i:1}"
        sleep "0.0$((RANDOM % 5 + 2))"
    done
    sleep 0.5
    printf '\n'
}

# Type a command, then run exactly what was typed. Commands that report a
# problem on purpose (a DEGRADED path, wtf's warnings) exit non-zero; the
# tour carries on, as a person would.
show() {
    type_command "$1"
    eval "$1" || true
    printf '\n'
    sleep 1.5
}

# Poll a check function until it succeeds, then say how long it took.
# $1: what we're waiting for, $2: timeout in seconds, $3...: the check.
wait_for() {
    local what="$1" limit="$2" start elapsed
    shift 2
    start=$(date +%s)
    say "waiting for ${what}"
    until "$@"; do
        elapsed=$(( $(date +%s) - start ))
        if [[ "${elapsed}" -ge "${limit}" ]]; then
            say "gave up after ${elapsed} s"
            exit 1
        fi
        sleep 2
    done
    elapsed=$(( $(date +%s) - start ))
    say "done after ${elapsed} s"
}

# Running instances of an app, from `relish status` (NODE INSTANCE APP
# NAMESPACE STATE ...). $2, if given, leaves out a node by name suffix.
running() {
    relish status 2>/dev/null \
        | awk -v app="$1" -v skip="${2:-}" \
            '$3 == app && $5 == "running" && (skip == "" || $1 !~ skip "$") { n++ } END { print n + 0 }'
}

all_apps_running() {
    [[ $(running frontend) -ge 3 && $(running backend) -ge 1 \
        && $(running redis) -ge 1 && $(running loadgen) -ge 1 ]]
}

path_passes() {
    relish path frontend --to redis >/dev/null 2>&1
}

metrics_scraped() {
    relish metrics frontend 2>/dev/null \
        | awk '$1 == "http_requests_total" && $4 >= 3 { found = 1 } END { exit !found }'
}

frontend_restarted() {
    relish status 2>/dev/null \
        | awk '$3 == "frontend" && $5 == "running" { n++; if ($7 >= 1) r = 1 } END { exit !(n >= 3 && r) }'
}

three_frontends_without_node_3() {
    [[ $(running frontend '-3') -ge 3 ]]
}

run_dashboard() {
    local log pid
    type_command "relish dashboard --no-open"
    log=$(mktemp)
    relish dashboard --no-open >"${log}" 2>&1 &
    pid=$!
    until grep -q 'http://' "${log}" 2>/dev/null; do
        kill -0 "${pid}" 2>/dev/null || break
        sleep 0.5
    done
    cat "${log}"
    sleep 4
    kill -INT "${pid}" 2>/dev/null || true
    wait "${pid}" 2>/dev/null || true
    printf '^C\n\n'
    rm -f "${log}"
    sleep 1
}

say "The five-minute tour from reliaburger.com, on a real three-node laptop cluster,"
say "run by scripts/demo/tour.sh. It waits on the cluster's real state where the tour"
say "says to wait, and says how long each wait took."
if [[ -n "${RECORDING}" ]]; then
    say "This recording cuts idle time to ${IDLE_LIMIT} s, so those numbers are the real ones."
fi
printf '\n'
sleep 2

# The commands arrive on descriptor 3, so a command that reads standard input
# can't swallow the rest of the tour.
previous=""
while IFS= read -r command <&3; do
    case "${command}" in
        "curl -fsSL https://reliaburger.com/install.sh | sh")
            if [[ -z "${SETUP_BINARIES}" ]]; then
                say "Step 1 ran before this recording: it installs relish and runs"
                say "\`relish setup --quickstart\`. The cluster is up."
                printf '\n'
            else
                say "Step 1, the install line, installs relish and runs \`relish setup --quickstart\`."
                say "The signed release isn't published yet, so this runs the same setup with"
                say "binaries built from this checkout. The times on the right are real."
                if [[ -n "${RECORDING}" ]]; then
                    say "This step plays ${SETUP_SPEEDUP}× faster than it ran."
                fi
                show "relish setup --quickstart --development-binaries ${SETUP_BINARIES}"
            fi
            ;;
        "relish apply -f ${DEMO_URL}")
            if curl -fsSI "${DEMO_URL}" >/dev/null 2>&1; then
                show "${command}"
            else
                say "${DEMO_URL} is published with the site; until then,"
                say "the same file from the repository:"
                show "relish apply -f ${DEMO_FILE}"
            fi
            ;;
        "relish status")
            case "${previous}" in
                "relish apply -f ${DEMO_URL}")
                    wait_for "the images to arrive and all four apps to run" 300 all_apps_running
                    show "${command}"
                    say "Step 4 opens ${INGRESS} in a browser. The same address from curl:"
                    show "for i in 1 2 3; do curl -s -H 'Accept: application/json' ${INGRESS} | grep hostname; done"
                    ;;
                "relish fault kill frontend --count 1 --acknowledge")
                    sleep 2
                    show "${command}"
                    wait_for "the killed replica to come back" 120 frontend_restarted
                    show "${command}"
                    ;;
                "relish local stop node-3")
                    wait_for "three frontends on the two surviving nodes" 240 three_frontends_without_node_3
                    show "${command}"
                    ;;
                *)
                    show "${command}"
                    ;;
            esac
            ;;
        "relish path frontend --to redis")
            wait_for "redis to reach the service map" 120 path_passes
            show "${command}"
            ;;
        "relish metrics frontend")
            wait_for "a few scrapes from every replica" 120 metrics_scraped
            show "${command}"
            ;;
        "relish metrics frontend --name http_request_duration_seconds")
            say "twenty seconds for the slow requests to reach the metrics, as the tour says"
            sleep 20
            show "${command}"
            ;;
        "relish dashboard")
            say "--no-open so no browser pops up in the recording; without it, relish opens"
            say "this address for you. Ctrl-C closes it; the cluster keeps running."
            run_dashboard
            ;;
        "relish local destroy --yes" | "relish uninstall" | "relish manual tour")
            # Cleanup and the pointer to the manual end the tour; the
            # recording stops before them.
            ;;
        *)
            if ! known_command "${command}"; then
                echo "scripts/demo/tour.sh doesn't know how to run: ${command}" >&2
                exit 1
            fi
            show "${command}"
            ;;
    esac
    previous="${command}"
done 3< <(tour_commands)

say "That's the tour. \`relish local destroy --yes\` removes the cluster,"
say "and \`relish uninstall\` removes relish itself."
sleep 3
