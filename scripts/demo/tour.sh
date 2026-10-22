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
#   scripts/demo/tour.sh [--install VERSION | --setup DEV_BINARIES_DIR]
#       Run the tour. With neither option it needs a running quickstart
#       cluster and starts at `relish apply`. With --install it runs the
#       install line for real, with RELIABURGER_VERSION=VERSION so it fetches
#       that published release whatever the live site's default is, then puts
#       the installed relish on PATH. With --setup it builds the cluster with
#       `relish setup --quickstart --development-binaries DEV_BINARIES_DIR`,
#       the same setup the install line runs, with binaries from this checkout.
#   scripts/demo/tour.sh --record CAST [--install VERSION | --setup DEV_BINARIES_DIR]
#       Record the run with asciinema into CAST (110x32, idle time cut to
#       2 s), then play the setup step SETUP_SPEEDUP (4) times faster. Setup
#       redraws its timers several times a second, so idle trimming alone
#       would leave a minute and a half of VM boots. Both are said on screen.
#   ... [--api-port PORT] [--ingress-port PORT] [--registry-port PORT]
#       Give the tour's cluster these host ports instead of the defaults
#       (19117, 18080, 15050), so it can run beside another laptop cluster.
#       --install and --setup pass them to `relish setup --quickstart`, and
#       the tour's requests use the ingress port. The recording says so and
#       shows the commands with the ports it really used.
#
# Once the cluster exists, scripts/demo/resources.py samples every node VM
# every five seconds (#671): Bun's memory and CPU, the whole guest's, and the
# guest's network traffic. Each wait ends with a one-line readout of the
# latest sample, so a recording shows the cluster's footprint at the moment
# it describes, and the tour ends with each measure's average and peak.
#
# Environment:
#   RELISH             the relish binary to run (default: the one on PATH)
#   RELIABURGER_HOME   where --install puts relish (default: ~/.reliaburger)
#   DEMO_RESOURCES=0   don't measure the tour's resource use
#   DEMO_RESOURCES_OUT keep the samples there as resources.csv and steps.tsv,
#                      with cpu.svg, memory.svg and network.svg charts
#
# The script needs bash 3.2 (macOS's /bin/bash), so no associative arrays.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
PAGE="${REPO_DIR}/docs/website/index.html"
DEMO_URL="https://reliaburger.com/demo/podinfo.yaml"
DEMO_FILE="examples/kubernetes/podinfo.yaml"
BURGER_URL="https://reliaburger.com/demo/burger.tar.gz"
BURGER_FETCH="curl -fsSL ${BURGER_URL} | tar xz"
# The order request as the page shows it, on the default ingress port.
BURGER_ORDER="http://burger.localhost:18080/order"

IDLE_LIMIT=2
SETUP_SPEEDUP=4

BATCH_ID=""
MODE="run"
SETUP_BINARIES=""
INSTALL_VERSION=""
CAST=""
INGRESS_PORT=18080
# Port options for `relish setup --quickstart`, each with a leading space.
SETUP_PORTS=""
port_number() {
    [[ "$2" =~ ^[0-9]+$ ]] || { echo "$1 needs a port number" >&2; exit 64; }
}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --check) MODE="check"; shift ;;
        --setup) SETUP_BINARIES="${2:?--setup needs a directory}"; shift 2 ;;
        --install) INSTALL_VERSION="${2:?--install needs a version, such as v0.1.1}"; shift 2 ;;
        --record) MODE="record"; CAST="${2:?--record needs a file}"; shift 2 ;;
        --api-port | --registry-port)
            port_number "$1" "${2:-}"; SETUP_PORTS="${SETUP_PORTS} $1 $2"; shift 2 ;;
        --ingress-port)
            port_number "$1" "${2:-}"; INGRESS_PORT="$2"; SETUP_PORTS="${SETUP_PORTS} $1 $2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 64 ;;
    esac
done
if [[ -n "${SETUP_BINARIES}" && -n "${INSTALL_VERSION}" ]]; then
    echo "--install and --setup are alternatives; pass one" >&2
    exit 64
fi
INGRESS="http://podinfo.localhost:${INGRESS_PORT}"
BURGER_ORDER_SENT="http://burger.localhost:${INGRESS_PORT}/order"
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
        "curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop" \
        | "relish apply -f ${DEMO_URL}" \
        | "relish status" \
        | "relish council status" \
        | "${BURGER_FETCH}" \
        | "relish build burger/burger.toml" \
        | "relish images" \
        | "relish apply burger/burger.toml" \
        | "curl ${BURGER_ORDER}" \
        | "relish batch submit burger/jobs.toml" \
        | "relish batch watch 1" \
        | "relish jobs" \
        | "relish path frontend --to redis" \
        | "relish metrics frontend" \
        | "relish fault delay redis 300ms --from frontend --duration 2m --acknowledge" \
        | "relish path frontend --to redis --count 3" \
        | "relish metrics frontend --name http_request_duration_seconds" \
        | "relish dashboard" \
        | "relish fault kill frontend --count 1 --acknowledge" \
        | "relish local stop node-3" \
        | "relish inspect frontend" \
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
    elif [[ -n "${INSTALL_VERSION}" ]]; then
        inner="${inner} --install ${INSTALL_VERSION}"
    fi
    inner="${inner}${SETUP_PORTS}"
    # The title names the release the recording installed, so
    # tests/suite/website.rs can check it against install.sh's default.
    TITLE_VERSION="development build"
    if [[ -n "${INSTALL_VERSION}" && -z "${SETUP_BINARIES}" ]]; then
        TITLE_VERSION="${INSTALL_VERSION#v}"
    fi
    TOUR_RECORDING=1 asciinema rec --headless --overwrite --return \
        --window-size 110x32 --idle-time-limit "${IDLE_LIMIT}" \
        --title "Reliaburger ${TITLE_VERSION}: the five-minute tour" -c "${inner}" "${CAST}"
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
if [[ -n "${INSTALL_VERSION}" ]]; then
    # Where the installer puts relish; the installed one must win over any
    # other relish on PATH.
    PATH="${RELIABURGER_HOME:-${HOME}/.reliaburger}/bin:${PATH}"
    export RELIABURGER_VERSION="${INSTALL_VERSION}"
elif [[ -n "${RELISH:-}" ]]; then
    PATH="$(cd "$(dirname "${RELISH}")" && pwd):${PATH}"
fi
if [[ -z "${INSTALL_VERSION}" ]]; then
    command -v relish >/dev/null || { echo "relish isn't on PATH; set RELISH" >&2; exit 1; }
fi

# The build step unpacks burger/ into the current directory. It runs in a
# scratch directory instead, so the checkout stays clean; `examples` there
# points back at the repository for the unpublished-tarball fallback.
BURGER_WORK=$(mktemp -d)
ln -s "${REPO_DIR}/examples" "${BURGER_WORK}/examples"

RESOURCES="${REPO_DIR}/scripts/demo/resources.py"
RESOURCES_HOME="${RELIABURGER_HOME:-${HOME}/.reliaburger}"
SAMPLER_PID=""
SAMPLER_START=""

stop_sampling() {
    if [[ -n "${SAMPLER_PID}" ]]; then
        kill -TERM "${SAMPLER_PID}" 2>/dev/null || true
        wait "${SAMPLER_PID}" 2>/dev/null || true
        SAMPLER_PID=""
    fi
}
trap 'stop_sampling; rm -rf "${BURGER_WORK}"' EXIT

# Start sampling the quickstart cluster's VMs, once it exists. Without
# python3 or a cluster the tour runs unmeasured.
start_sampling() {
    if [[ -n "${SAMPLER_PID}" || "${DEMO_RESOURCES:-1}" == 0 ]] \
        || [[ ! -f "${RESOURCES_HOME}/clusters/laptop/state.json" ]] \
        || ! command -v python3 >/dev/null; then
        return 0
    fi
    SAMPLER_START=$(date +%s)
    printf '0\tcluster up\n' >"${BURGER_WORK}/steps.tsv"
    python3 "${RESOURCES}" sample --quickstart "${RESOURCES_HOME}" --interval 5 \
        --start "${SAMPLER_START}" --csv "${BURGER_WORK}/resources.csv" 2>"${BURGER_WORK}/resources.log" &
    SAMPLER_PID=$!
}

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

# The same, in the build step's scratch directory.
show_in_work() {
    type_command "$1"
    (cd "${BURGER_WORK}" && eval "$1") || true
    printf '\n'
    sleep 1.5
}

# Whether the published demo archive holds exactly the repository's
# examples/demo/burger, so the tour can show the real download.
live_burger_matches_repository() {
    local live
    live=$(mktemp -d)
    curl -fsSL "${BURGER_URL}" 2>/dev/null | tar xz -C "${live}" 2>/dev/null \
        && diff -rq "${live}/burger" "${REPO_DIR}/examples/demo/burger" >/dev/null 2>&1
    local matches=$?
    rm -rf "${live}"
    return "${matches}"
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
    resource_readout "${what}"
}

# Note step $1 for the charts and narrate the latest sample.
resource_readout() {
    local line
    if [[ -z "${SAMPLER_PID}" ]]; then
        return 0
    fi
    printf '%s\t%s\n' "$(( $(date +%s) - SAMPLER_START ))" "$1" >>"${BURGER_WORK}/steps.tsv"
    line=$(python3 "${RESOURCES}" readout --csv "${BURGER_WORK}/resources.csv" 2>/dev/null || true)
    if [[ -n "${line}" ]]; then
        say "${line}"
    fi
}

# The average and peak of each measure, narrated; the samples and charts are
# kept in DEMO_RESOURCES_OUT if it's set.
resource_summary() {
    local line
    if [[ -z "${SAMPLER_PID}" ]]; then
        return 0
    fi
    stop_sampling
    while IFS= read -r line; do
        say "${line}"
    done < <(python3 "${RESOURCES}" summary --csv "${BURGER_WORK}/resources.csv")
    if [[ -n "${DEMO_RESOURCES_OUT:-}" ]]; then
        mkdir -p "${DEMO_RESOURCES_OUT}"
        cp "${BURGER_WORK}/resources.csv" "${BURGER_WORK}/steps.tsv" "${DEMO_RESOURCES_OUT}/"
        python3 "${RESOURCES}" chart --csv "${BURGER_WORK}/resources.csv" \
            --steps "${BURGER_WORK}/steps.tsv" --out "${DEMO_RESOURCES_OUT}" >/dev/null || true
    fi
    printf '\n'
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

two_burgers_running() {
    [[ $(running burger) -ge 2 ]]
}

burger_takes_orders() {
    curl -fsS "${BURGER_ORDER_SENT}" >/dev/null 2>&1
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
if [[ -n "${SETUP_PORTS}" ]]; then
    say "Another laptop cluster holds the default ports, so this one uses${SETUP_PORTS}."
fi
printf '\n'
sleep 2

# The commands arrive on descriptor 3, so a command that reads standard input
# can't swallow the rest of the tour.
previous=""
while IFS= read -r command <&3; do
    start_sampling
    case "${command}" in
        "curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop")
            if [[ -n "${INSTALL_VERSION}" ]]; then
                say "Step 1 installs the published ${INSTALL_VERSION} release, then runs"
                say "\`relish setup --quickstart\`. The times on the right are real."
                if [[ -n "${RECORDING}" ]]; then
                    say "This step plays ${SETUP_SPEEDUP}× faster than it ran."
                fi
                if [[ -n "${SETUP_PORTS}" ]]; then
                    show "${command}${SETUP_PORTS}"
                else
                    show "${command}"
                fi
            elif [[ -z "${SETUP_BINARIES}" ]]; then
                say "Step 1 ran before this recording: it installs relish and runs"
                say "\`relish setup --quickstart\`. The cluster is up."
                printf '\n'
            else
                say "Step 1, the install line, installs relish and runs \`relish setup --quickstart\`."
                say "This recording runs the same setup with binaries built from this checkout."
                say "The times on the right are real."
                if [[ -n "${RECORDING}" ]]; then
                    say "This step plays ${SETUP_SPEEDUP}× faster than it ran."
                fi
                show "relish setup --quickstart --development-binaries ${SETUP_BINARIES}${SETUP_PORTS}"
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
                *)
                    show "${command}"
                    ;;
            esac
            ;;
        "${BURGER_FETCH}")
            # The site publishes the demo from the tag the installer fetches,
            # so until a promotion's site change deploys, the live archive is
            # the previous release's: published, but not what this tour uses
            # (0.1.6's had no jobs.toml, and 0.2.0's ran 1,064 tasks).
            if live_burger_matches_repository; then
                show_in_work "${command}"
            else
                say "${BURGER_URL} is published with the site, and this release's"
                say "copy isn't there yet; the same directory from the repository:"
                show_in_work "cp -R examples/demo/burger ."
            fi
            ;;
        "relish batch submit burger/jobs.toml")
            # The page's later commands name manifest 1, as on a fresh
            # cluster; the script reads the number submit really printed.
            type_command "${command}"
            (cd "${BURGER_WORK}" && relish batch submit burger/jobs.toml) | tee "${BURGER_WORK}/submit.log"
            BATCH_ID=$(sed -n 's/^manifest \([0-9][0-9]*\) submitted.*/\1/p' "${BURGER_WORK}/submit.log")
            [[ -n "${BATCH_ID}" ]] || { echo "batch submit printed no manifest number" >&2; exit 1; }
            printf '\n'
            sleep 1.5
            ;;
        "relish batch watch 1")
            type_command "relish batch watch ${BATCH_ID}"
            # watch prints a summary a second, live; meanwhile the script
            # checks that the burger app keeps taking orders.
            relish batch watch "${BATCH_ID}" &
            WATCH_PID=$!
            while kill -0 "${WATCH_PID}" 2>/dev/null; do
                if ! burger_takes_orders; then
                    kill -TERM "${WATCH_PID}" 2>/dev/null || true
                    wait "${WATCH_PID}" 2>/dev/null || true
                    echo "burger stopped serving during the batch" >&2; exit 1
                fi
                sleep 1
            done
            wait "${WATCH_PID}"
            relish --output json batch-status "${BATCH_ID}" >"${BURGER_WORK}/summary.json"
            python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); assert s["done"] and s["succeeded"]==10 and s["failed"]==0 and s["not_run"]==0, s' "${BURGER_WORK}/summary.json"
            say "All 10 tasks succeeded; the burger app kept serving orders."
            resource_readout "the batch"
            printf '\n'
            sleep 1.5
            ;;
        "relish build burger/burger.toml" | "relish apply burger/burger.toml")
            show_in_work "${command}"
            ;;
        "curl ${BURGER_ORDER}")
            wait_for "both burger replicas to run" 180 two_burgers_running
            wait_for "the ingress to route burger.localhost" 60 burger_takes_orders
            show "curl ${BURGER_ORDER_SENT}"
            ;;
        "relish inspect frontend")
            wait_for "three frontends on the two surviving nodes" 240 three_frontends_without_node_3
            show "${command}"
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

resource_summary
say "That's the tour. \`relish local destroy --yes\` removes the cluster,"
say "and \`relish uninstall\` removes relish itself."
sleep 3
