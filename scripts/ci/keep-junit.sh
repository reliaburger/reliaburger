#!/usr/bin/env bash
# Keep one suite's JUnit report under its own name, with where it came from.
#
#   scripts/ci/keep-junit.sh SUITE COMMAND...
#
# Every nextest run with the ci profile writes target/nextest/ci/junit.xml, so
# a job that runs several suites would otherwise upload only the last one.
# This moves the report to target/junit/SUITE.xml (moving, not copying, so a
# later suite that writes nothing can't pass off this one's report) and
# records SUITE.meta.txt: the command, the commit, the run and the host.
# It fails when the suite wrote no report or an empty one, which is the
# "every suite produced its own evidence" check.
#
# Fixtures: scripts/ci/test_keep_junit.py.
set -euo pipefail

if [ "$#" -lt 2 ]; then
    echo "usage: $0 SUITE COMMAND..." >&2
    exit 2
fi
suite=$1
shift
source_report=${JUNIT_SOURCE:-target/nextest/ci/junit.xml}
kept=${JUNIT_DIR:-target/junit}

if [ ! -s "$source_report" ]; then
    echo "$suite wrote no JUnit report at $source_report" >&2
    exit 1
fi
mkdir -p "$kept"
if [ -e "$kept/$suite.xml" ]; then
    echo "a report for $suite is already kept in $kept" >&2
    exit 1
fi
mv "$source_report" "$kept/$suite.xml"

nextest=$(cargo nextest --version 2>/dev/null | head -n 1 || true)
{
    echo "suite: $suite"
    echo "command: $*"
    echo "commit: $(git rev-parse HEAD)"
    echo "ref: ${GITHUB_REF:-local}"
    echo "run: ${GITHUB_SERVER_URL:-}${GITHUB_RUN_ID:+/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/${GITHUB_RUN_ATTEMPT:-1}}"
    echo "job: ${GITHUB_JOB:-local}"
    echo "host: $(uname -srm)"
    echo "runner: ${RUNNER_OS:-local} ${RUNNER_ARCH:-}"
    echo "nextest: ${nextest:-unknown}"
} >"$kept/$suite.meta.txt"
echo "kept $kept/$suite.xml"
