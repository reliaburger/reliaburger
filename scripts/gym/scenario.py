"""Load and check the gym's scenario files (stdlib only).

A scenario is one TOML file in scripts/gym/scenarios/: what the cluster looks
like before the solver starts (`setup`), what went wrong (an app manifest, a
Smoker fault or a planted bug), what the solver is told (`prompt`), the right
answer (`truth`) and which parts of an answer count (`score.checks`).

Everything is checked here, before a cluster starts, so a typo in a scenario
fails `gym.py check` (and `make test-ci-scripts`) rather than half an hour
into a run.
"""
from dataclasses import dataclass
from pathlib import Path
import re
import tomllib

GYM_DIR = Path(__file__).resolve().parent
SCENARIO_DIR = GYM_DIR / "scenarios"
# The Kubernetes ports of the scenarios (#1679): the same ids, the same truth
# and the same checks, set up on a k3s lab instead of a Reliaburger cluster.
K8S_SCENARIO_DIR = SCENARIO_DIR / "k8s"

# The platforms a scenario can run on. `reliaburger` is the default, so a file
# without a `platform` key is a Reliaburger scenario.
PLATFORMS = ("reliaburger", "k8s")

# The closed list of fault kinds an answer may name. The prompt lists it, so
# scoring compares two words instead of judging prose.
FAULT_KINDS = (
    "crash_loop",
    "health_check_failing",
    "unschedulable",
    "node_partitioned",
    "relish_bug",
    "none",
    "other",
)

# The fields of an answer a scenario can score, and `patch`, which is scored
# by applying it to the planted tree and running the reverted tests.
CHECKS = ("app", "namespace", "node", "fault_kind", "file", "function", "patch")

# A truth value that starts with `@` names a fact the runner learns from the
# live cluster: `@instance:web` is the node running web's first instance,
# `@follower` the council follower the runner chose for a node fault.
FACT_PATTERN = re.compile(r"^@(instance:[a-z0-9][a-z0-9-]*|follower|leader)$")
# On Kubernetes there's no council: `@worker` is the agent node the runner
# chose to partition (the server, like the Raft leader, stays reachable).
K8S_FACT_PATTERN = re.compile(r"^@(instance:[a-z0-9][a-z0-9-]*|worker)$")

# The faults a scenario can inject at setup, per platform, and the fact each
# one targets. Reliaburger's is a Smoker fault through the API; Kubernetes has
# no fault injector, so the k3s lab drops the node's traffic with iptables.
FAULT_STEPS = {
    "reliaburger": {"council_partition": "@follower"},
    "k8s": {"node_partition": "@worker"},
}

DURATION_PATTERN = re.compile(r"^([0-9]+)(ms|s|m)$")


class ScenarioError(ValueError):
    """A scenario file that can't be run, with the file and the reason."""


@dataclass(frozen=True)
class SetupStep:
    """One thing the runner does, as admin, before the solver starts.

    Exactly one of `apply`, `fault` or `settle` is set. `wait_running`
    names an app whose instances must all run before the next step.
    """

    apply: str | None = None
    wait_running: str | None = None
    fault: dict | None = None
    settle_seconds: float | None = None


@dataclass(frozen=True)
class Plant:
    """A deliberate defect, applied to a throwaway copy of the source.

    The patch never touches this checkout: the runner extracts the source
    with `relish agent source extract`, applies the patch there and builds
    the solver's `relish` from that copy.
    """

    patch: Path
    reverted_tests: tuple[str, ...]
    # The `relish` arguments that show the bug: the runner runs them with the
    # shipped and the planted binary before the solver starts, and a run
    # where only the planted one shows it is the only valid run.
    symptom: tuple[str, ...]


@dataclass(frozen=True)
class Injection:
    """Text an app logs to try to steer the solver.

    `canary` is a unique string in every injected line, so the runner can
    find where the text travelled. `decoys` are what the text tells the
    solver to blame; an answer that names one was steered.
    """

    canary: str
    decoys: tuple[str, ...]


@dataclass(frozen=True)
class ReportCheck:
    """A report the solver must draft without leaking what was planted.

    `secrets` are strings planted on the cluster (an env value, a token and
    an address in a log line) that must appear in no file of any draft;
    `counted` are the redaction rules the drafts' manifests must count at
    least once between them, which shows the secrets went in and were
    caught rather than left out.
    """

    secrets: tuple[str, ...]
    counted: tuple[str, ...]
    # Where the draft comes from: `files`, the directories `relish agent
    # report` wrote, or `answer`, the Markdown in the answer's `report` field
    # (Kubernetes has no report command, so the agent writes the draft).
    source: str = "files"


# The redaction rules a report's MANIFEST.json counts.
REPORT_RULES = (
    "reliaburger_token", "encrypted_value", "age_identity", "pem_private_key",
    "url_credentials", "env_value", "hostname", "ipv4_address", "ipv6_address",
    "node_name",
)


@dataclass(frozen=True)
class Scenario:
    """One scenario file, checked."""

    id: str
    title: str
    path: Path
    nodes: int
    prompt: str
    setup: tuple[SetupStep, ...]
    truth: dict
    checks: tuple[str, ...]
    required_calls: tuple[str, ...] = ()
    plant: Plant | None = None
    injection: Injection | None = None
    report: ReportCheck | None = None
    platform: str = "reliaburger"
    # For a port: what differs from the Reliaburger scenario, in a sentence.
    ported: str | None = None

    def facts_needed(self):
        """The `@` facts this scenario's truth refers to."""
        return sorted(
            value[1:]
            for value in self.truth.values()
            if isinstance(value, str) and value.startswith("@")
        )


def parse_duration(text):
    """Seconds in "500ms", "20s" or "2m"."""
    match = DURATION_PATTERN.match(text)
    if not match:
        raise ValueError(f"{text!r} isn't a duration like 500ms, 20s or 2m")
    number, unit = int(match.group(1)), match.group(2)
    return {"ms": number / 1000, "s": number, "m": number * 60}[unit]


def _fail(path, message):
    raise ScenarioError(f"{path.name}: {message}")


def _setup_step(path, index, raw, platform="reliaburger"):
    where = f"setup step {index + 1}"
    if not isinstance(raw, dict):
        _fail(path, f"{where} isn't a table")
    actions = [key for key in ("apply", "fault", "settle") if key in raw]
    if len(actions) != 1:
        _fail(path, f"{where} needs exactly one of apply, fault or settle, got {actions or 'none'}")
    unknown = set(raw) - {"apply", "fault", "settle", "wait_running"}
    if unknown:
        _fail(path, f"{where} has unknown keys {sorted(unknown)}")
    if "wait_running" in raw and "apply" not in raw:
        _fail(path, f"{where}: wait_running goes with apply")
    if "apply" in raw:
        manifest = GYM_DIR / raw["apply"]
        if not manifest.is_file():
            _fail(path, f"{where} applies {raw['apply']}, which doesn't exist")
        wanted = ".yaml" if platform == "k8s" else ".toml"
        if manifest.suffix != wanted:
            _fail(path, f"{where}: a {platform} scenario applies {wanted} manifests")
        return SetupStep(apply=raw["apply"], wait_running=raw.get("wait_running"))
    if "settle" in raw:
        try:
            return SetupStep(settle_seconds=parse_duration(raw["settle"]))
        except ValueError as error:
            _fail(path, f"{where}: {error}")
    fault = raw["fault"]
    kinds = FAULT_STEPS[platform]
    if not isinstance(fault, dict) or fault.get("kind") not in kinds:
        _fail(path, f"{where}: fault.kind must be one of {list(kinds)}")
    target = kinds[fault["kind"]]
    if fault.get("target") != target:
        _fail(path, f"{where}: a {fault['kind']} targets {target}")
    try:
        parse_duration(fault.get("duration", ""))
    except ValueError as error:
        _fail(path, f"{where}: fault.duration: {error}")
    return SetupStep(fault=dict(fault))


def load(path):
    """Read and check one scenario file; raise ScenarioError naming the problem."""
    path = Path(path)
    try:
        raw = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as error:
        _fail(path, f"isn't valid TOML: {error}")
    known = {
        "id", "title", "nodes", "prompt", "setup", "truth", "score", "plant",
        "injection", "report", "platform", "ported",
    }
    unknown = set(raw) - known
    if unknown:
        _fail(path, f"unknown keys {sorted(unknown)}")
    for key in ("id", "title", "prompt"):
        if not isinstance(raw.get(key), str) or not raw[key].strip():
            _fail(path, f"`{key}` must be a non-empty string")
    if raw["id"] != path.stem:
        _fail(path, f"id {raw['id']!r} must match the file name {path.stem!r}")
    platform = raw.get("platform", "reliaburger")
    if platform not in PLATFORMS:
        _fail(path, f"`platform` must be one of {list(PLATFORMS)}")
    ported = raw.get("ported")
    if platform == "k8s" and (not isinstance(ported, str) or not ported.strip()):
        _fail(path, "a k8s scenario needs `ported`: what differs from the Reliaburger one")
    nodes = raw.get("nodes", 1)
    if not isinstance(nodes, int) or not 1 <= nodes <= 5:
        _fail(path, "`nodes` must be between 1 and 5")

    setup = tuple(
        _setup_step(path, i, step, platform) for i, step in enumerate(raw.get("setup", []))
    )
    if any(step.fault for step in setup) and nodes < 3:
        _fail(path, "a partition needs at least three nodes")

    truth = raw.get("truth")
    if not isinstance(truth, dict) or not truth:
        _fail(path, "`truth` must be a table")
    # A patch has no true value: it's right when the reverted tests pass.
    unknown = set(truth) - (set(CHECKS) - {"patch"})
    if unknown:
        _fail(path, f"truth has unknown fields {sorted(unknown)}")
    if truth.get("fault_kind") not in FAULT_KINDS:
        _fail(path, f"truth.fault_kind must be one of {list(FAULT_KINDS)}")
    facts = K8S_FACT_PATTERN if platform == "k8s" else FACT_PATTERN
    for name, value in truth.items():
        if isinstance(value, str) and value.startswith("@") and not facts.match(value):
            _fail(path, f"truth.{name} = {value!r} isn't a fact the runner knows")

    score = raw.get("score", {})
    checks = tuple(score.get("checks", ()))
    if not checks:
        _fail(path, "score.checks must name at least one check")
    for check in checks:
        if check not in CHECKS:
            _fail(path, f"score.checks: unknown check {check!r}")
        if check != "patch" and check not in truth:
            _fail(path, f"score.checks names {check!r}, which truth doesn't give")
    required_calls = tuple(score.get("required_calls", ()))

    plant = None
    if "plant" in raw and platform != "reliaburger":
        # The planted bug is a bug in Reliaburger's own source: Kubernetes has
        # no equivalent loop, and the comparison says so rather than scoring it.
        _fail(path, "a plant is a Reliaburger bug: only a reliaburger scenario can carry one")
    if "plant" in raw:
        patch = GYM_DIR / raw["plant"].get("patch", "")
        if not patch.is_file():
            _fail(path, f"plant.patch {raw['plant'].get('patch')!r} doesn't exist")
        tests = tuple(raw["plant"].get("reverted_tests", ()))
        if not tests:
            _fail(path, "plant.reverted_tests must name the tests the plant breaks")
        symptom = tuple(raw["plant"].get("symptom", ()))
        if not symptom:
            _fail(path, "plant.symptom must give the relish arguments that show the bug")
        plant = Plant(patch=patch, reverted_tests=tests, symptom=symptom)
    if "patch" in checks and plant is None:
        _fail(path, "the patch check needs a plant to apply it to")

    injection = None
    if "injection" in raw:
        canary = raw["injection"].get("canary", "")
        if not re.fullmatch(r"[A-Z0-9-]{8,}", canary):
            _fail(path, "injection.canary must be 8 or more of A-Z, 0-9 and -")
        decoys = tuple(raw["injection"].get("decoys", ()))
        if not decoys:
            _fail(path, "injection.decoys must name what the text tries to blame")
        for name in ("app", "node"):
            if truth.get(name) in decoys:
                _fail(path, f"a decoy can't be the true {name}")
        injection = Injection(canary=canary, decoys=decoys)

    report = None
    if "report" in raw:
        secrets = tuple(raw["report"].get("secrets", ()))
        if not secrets or not all(isinstance(s, str) and len(s) >= 8 for s in secrets):
            _fail(path, "report.secrets must list the planted strings, each 8 or more characters")
        counted = tuple(raw["report"].get("counted", ()))
        source = raw["report"].get("source", "files")
        if source not in ("files", "answer"):
            _fail(path, "report.source must be files or answer")
        unknown = sorted(set(counted) - set(REPORT_RULES))
        # A draft the agent wrote itself has no redaction manifest to count.
        if unknown or (source == "files" and not counted) or (source == "answer" and counted):
            _fail(path, "report.counted must name redaction rules from "
                  f"{list(REPORT_RULES)} for a files draft, and none for an answer draft")
        report = ReportCheck(secrets=secrets, counted=counted, source=source)

    return Scenario(
        id=raw["id"],
        title=raw["title"],
        path=path,
        nodes=nodes,
        prompt=raw["prompt"].strip(),
        setup=setup,
        truth=dict(truth),
        checks=checks,
        required_calls=required_calls,
        plant=plant,
        injection=injection,
        report=report,
        platform=platform,
        ported=ported.strip() if isinstance(ported, str) else None,
    )


def directory_for(platform):
    """Where a platform's scenarios live."""
    return K8S_SCENARIO_DIR if platform == "k8s" else SCENARIO_DIR


def load_all(directory=None, platform="reliaburger"):
    """Every scenario for `platform` (or in `directory`), in file-name order."""
    directory = Path(directory) if directory is not None else directory_for(platform)
    paths = sorted(directory.glob("*.toml"))
    if not paths:
        raise ScenarioError(f"no scenarios in {directory}")
    loaded = [load(path) for path in paths]
    wrong = [s.id for s in loaded if s.platform != platform]
    if wrong:
        raise ScenarioError(f"{directory} holds scenarios for another platform: {wrong}")
    return loaded
