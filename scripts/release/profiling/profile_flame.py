#!/usr/bin/env python3
"""Fold `perf script` output and render flamegraph SVGs, standard library only.

The release profile step (scripts/release/qualify-profile.sh) runs it inside
the bench VM to fold stacks and on the host to draw them. It started as the
2026-10-20 host-job profile record's tools/flame.py.

  profile_flame.py fold-cpu PERF_SCRIPT_TXT [--tgid N]     > folded  (one line per stack: "a;b;c count")
  profile_flame.py fold-off PERF_SCRIPT_TXT --tgid N|--comm C > folded (sched_switch: weight = off-CPU microseconds)
  profile_flame.py svg FOLDED --title T [--unit samples|us] [--min-pct 0.2] > out.svg
  profile_flame.py diff BEFORE AFTER --title T [--min-pct 0.2] > out.svg
                                       (AFTER's stacks, red where a frame's share grew, blue where it shrank)
  profile_flame.py top FOLDED [--n 40]                     (inclusive share per frame)
"""
import argparse, html, re, sys, zlib
from collections import defaultdict

HEADER = re.compile(r"^\s*(\S.*?)\s+(\d+)/(\d+)\s+(?:\[\d+\]\s+)?(\d+\.\d+):\s+(\S+?):?(?:\s+(.*))?$")
HEADER_NOTID = re.compile(r"^\s*(\S.*?)\s+(\d+)\s+(?:\[\d+\]\s+)?(\d+\.\d+):\s+(\S+?):?(?:\s+(.*))?$")


class V0:
    """A small Rust v0 symbol demangler (paths, generics and common types)."""

    BASIC = {"a": "i8", "b": "bool", "c": "char", "d": "f64", "e": "str", "f": "f32", "h": "u8",
             "i": "isize", "j": "usize", "l": "i32", "m": "u32", "n": "i128", "o": "u128", "s": "i16",
             "t": "u16", "u": "()", "v": "...", "x": "i64", "y": "u64", "z": "!", "p": "_"}

    def __init__(self, s):
        self.s, self.i, self.depth = s, 0, 0

    def peek(self):
        return self.s[self.i] if self.i < len(self.s) else ""

    def take(self):
        c = self.peek()
        self.i += 1
        return c

    def base62(self):
        if self.peek() == "_":
            self.i += 1
            return 0
        n = 0
        while self.peek() and self.peek() != "_":
            c = self.take()
            n = n * 62 + ("0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ".index(c))
        self.i += 1
        return n + 1

    def disambiguator(self):
        if self.peek() == "s":
            self.i += 1
            self.base62()

    def ident(self):
        self.disambiguator()
        if self.peek() == "u":
            self.i += 1
        start = self.i
        while self.peek().isdigit():
            self.i += 1
        n = int(self.s[start:self.i] or 0)
        if self.peek() == "_":
            self.i += 1
        name = self.s[self.i:self.i + n]
        self.i += n
        return name

    def backref(self, fn):
        pos = self.base62()
        saved = self.i
        self.i = pos
        self.depth += 1
        out = fn() if self.depth < 40 else "?"
        self.depth -= 1
        self.i = saved
        return out

    def path(self):
        c = self.take()
        if c == "C":
            return self.ident()
        if c == "N":
            ns = self.take()
            inner = self.path()
            name = self.ident()
            if ns == "C":
                return inner + "::{closure}"
            if ns == "S":
                return inner + "::{shim}"
            return inner + "::" + name if name else inner
        if c == "M":
            self.disambiguator()
            self.path()
            return "<" + self.type_() + ">"
        if c == "X":
            self.disambiguator()
            self.path()
            t = self.type_()
            return "<" + t + " as " + self.path() + ">"
        if c == "Y":
            t = self.type_()
            return "<" + t + " as " + self.path() + ">"
        if c == "I":
            p = self.path()
            args = []
            while self.peek() and self.peek() != "E":
                args.append(self.generic_arg())
            self.i += 1
            return p + "<" + ", ".join(a for a in args if a) + ">"
        if c == "B":
            return self.backref(self.path)
        raise ValueError(c)

    def generic_arg(self):
        c = self.peek()
        if c == "L":
            self.i += 1
            self.base62()
            return ""
        if c == "K":
            self.i += 1
            return self.const()
        return self.type_()

    def const(self):
        c = self.take()
        if c == "B":
            return self.backref(self.const)
        if c == "p":
            return "_"
        while self.peek() and self.peek() != "_":
            self.i += 1
        self.i += 1
        return "N"

    def type_(self):
        c = self.peek()
        if c in self.BASIC:
            self.i += 1
            return self.BASIC[c]
        self.i += 1
        if c in "RQ":
            if self.peek() == "L":
                self.i += 1
                self.base62()
            return ("&" if c == "R" else "&mut ") + self.type_()
        if c in "PO":
            return ("*const " if c == "P" else "*mut ") + self.type_()
        if c == "A":
            t = self.type_()
            self.const()
            return "[" + t + "; N]"
        if c == "S":
            return "[" + self.type_() + "]"
        if c == "T":
            ts = []
            while self.peek() and self.peek() != "E":
                ts.append(self.type_())
            self.i += 1
            return "(" + ", ".join(ts) + ")"
        if c == "F":
            if self.peek() == "G":
                self.i += 1
                self.base62()
            if self.peek() == "U":
                self.i += 1
            if self.peek() == "K":
                self.i += 1
                if self.peek() == "C":
                    self.i += 1
                else:
                    self.ident()
            while self.peek() and self.peek() != "E":
                self.type_()
            self.i += 1
            self.type_()
            return "fn(..)"
        if c == "D":
            if self.peek() == "G":
                self.i += 1
                self.base62()
            bounds = []
            while self.peek() and self.peek() != "E":
                p = self.path()
                while self.peek() == "p":
                    self.i += 1
                    self.ident()
                    self.type_()
                bounds.append(p)
            self.i += 1
            if self.peek() == "L":
                self.i += 1
                self.base62()
            return "dyn " + " + ".join(bounds)
        if c == "B":
            return self.backref(self.type_)
        self.i -= 1
        return self.path()


def demangle(sym):
    if not sym.startswith("_R"):
        return sym
    body = sym[2:]
    if body[:1].isdigit():
        body = body.lstrip("0123456789")
    try:
        out = V0(body).path()
    except (ValueError, IndexError, RecursionError):
        return sym
    out = re.sub(r"<([A-Za-z_:]+::)?\{closure\}>", "{closure}", out)
    return out


def clean(sym):
    sym = re.sub(r"\+0x[0-9a-f]+$", "", sym)
    sym = demangle(sym)
    sym = re.sub(r"::h[0-9a-f]{16}$", "", sym)
    sym = re.sub(r"<([^<>]*?) as ([^<>]*?)>", r"<\1 as \2>", sym)
    return sym.replace(";", ":")


def records(path):
    """Yield (comm, tgid, tid, time, event, rest, frames[leaf first])."""
    cur, frames = None, []
    with open(path, errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line.strip():
                if cur:
                    yield (*cur, frames)
                cur, frames = None, []
                continue
            if cur is None:
                m = HEADER.match(line)
                if m:
                    comm, tgid, tid, t, ev, rest = m.groups(default="")
                    cur = (comm, int(tgid), int(tid), float(t), ev, rest)
                else:
                    m = HEADER_NOTID.match(line)
                    if m:
                        comm, tid, t, ev, rest = m.groups(default="")
                        cur = (comm, -1, int(tid), float(t), ev, rest)
                continue
            parts = line.strip().split(None, 1)
            if len(parts) == 2:
                sym = parts[1].rsplit(" (", 1)[0]
                dso = parts[1].rsplit(" (", 1)[1].rstrip(")") if " (" in parts[1] else ""
                if sym == "[unknown]":
                    sym = "[" + dso.rsplit("/", 1)[-1] + "]" if dso else "[unknown]"
                frames.append(clean(sym))
    if cur:
        yield (*cur, frames)


def thread_group(comm):
    # tokio workers, blocking pool, etc. keep their names; collapse numeric suffixes
    return re.sub(r"[-_ ]?\d+$", "", comm)


def fold_cpu_stacks(path, tgid=0):
    """Count on-CPU samples per stack, root first, keyed "comm;frame;...;leaf"."""
    out = defaultdict(int)
    for comm, sample_tgid, tid, t, ev, rest, frames in records(path):
        if tgid and sample_tgid != tgid:
            continue
        stack = [thread_group(comm)] + list(reversed(frames))
        out[";".join(stack)] += 1
    return dict(out)


def fold_cpu(args):
    for k, v in sorted(fold_cpu_stacks(args.input, args.tgid).items()):
        print(f"{k} {v}")


def fold_off(args):
    out = defaultdict(float)
    pending = {}
    for comm, tgid, tid, t, ev, rest, frames in records(args.input):
        if "sched_switch" not in ev:
            continue
        m = re.search(r"next_pid=(\d+)", rest) or re.search(r"==> .*:(\d+) \[\d+\]\s*$", rest)
        nxt = int(m.group(1)) if m else None
        st = re.search(r"prev_state=(\S+)", rest) or re.search(r"\[\d+\] (\S+) ==>", rest)
        if tid != 0 and ((args.tgid and tgid == args.tgid) or (args.comm and comm == args.comm)):
            state = st.group(1) if st else "?"
            pending[tid] = (t, [thread_group(comm), f"state={state}"] + list(reversed(frames)))
        if nxt in pending:
            t0, stack = pending.pop(nxt)
            out[";".join(stack)] += (t - t0) * 1e6
    for k, v in sorted(out.items()):
        if v >= 1:
            print(f"{k} {int(v)}")


def load(path):
    stacks = []
    with open(path) as f:
        for line in f:
            s, _, n = line.rstrip("\n").rpartition(" ")
            if s:
                stacks.append((s.split(";"), float(n)))
    return stacks


def top(args):
    stacks = load(args.input)
    total = sum(n for _, n in stacks)
    incl, self_ = defaultdict(float), defaultdict(float)
    for frames, n in stacks:
        for fr in set(frames):
            incl[fr] += n
        self_[frames[-1]] += n
    print(f"total {total:.0f}")
    for fr, n in sorted(incl.items(), key=lambda x: -x[1])[: args.n]:
        print(f"{100 * n / total:6.2f}% incl {100 * self_[fr] / total:6.2f}% self  {fr[:160]}")


def timeline(args):
    """Samples per time bucket: idle, busybox (task work), and each --pattern found in a stack."""
    buckets = defaultdict(lambda: defaultdict(int))
    t0 = None
    for comm, tgid, tid, t, ev, rest, frames in records(args.input):
        t0 = t if t0 is None else t0
        b = int((t - t0) / args.bucket)
        row = buckets[b]
        row["all"] += 1
        if comm == "swapper":
            row["idle"] += 1
        if comm == "busybox":
            row["busybox"] += 1
        if comm == "helper":
            row["helper"] += 1
        joined = ";".join(clean(f) for f in frames) if args.pattern else ""
        for p in args.pattern:
            if p in joined:
                row[p] += 1
    cols = ["all", "idle", "busybox", "helper"] + args.pattern
    print("t(s) " + " | ".join(c[-28:] for c in cols))
    for b in sorted(buckets):
        print(f"{b * args.bucket:5.2f} " + " ".join(f"{buckets[b][c]:6d}" for c in cols))


def under(args):
    """Group the time beneath the last frame containing --frame by the next --depth frames."""
    stacks = load(args.input)
    total = sum(n for _, n in stacks)
    groups, matched = defaultdict(float), 0.0
    for frames, n in stacks:
        hits = [i for i, fr in enumerate(frames) if args.frame in fr]
        if not hits:
            continue
        matched += n
        rest = frames[hits[-1] + 1: hits[-1] + 1 + args.depth]
        groups[" > ".join(fr[:100] for fr in rest) or "(self)"] += n
    print(f"matched {matched:.0f} of {total:.0f} ({100 * matched / max(total, 1):.2f}%)")
    for k, n in sorted(groups.items(), key=lambda x: -x[1])[: args.n]:
        print(f"{100 * n / max(total, 1):6.2f}%  {k}")


def build_tree(stacks):
    """Merge folded stacks into a tree of {"n": weight, "c": {frame: child}}."""
    tree = {"n": 0.0, "c": {}}
    for frames, n in stacks:
        node = tree
        node["n"] += n
        for fr in frames:
            node = node["c"].setdefault(fr, {"n": 0.0, "c": {}})
            node["n"] += n
    return tree


def path_shares(stacks):
    """Each frame path's share of the total ("a;b" -> fraction), for differential colours."""
    total = sum(n for _, n in stacks) or 1.0
    shares = defaultdict(float)
    for frames, n in stacks:
        for depth in range(1, len(frames) + 1):
            shares[";".join(frames[:depth])] += n / total
    return shares


def hashed_fill(path, name):
    h = zlib.crc32(name.encode()) & 0xFFFF
    return f"rgb({205 + h % 50},{80 + (h >> 4) % 120},{40 + (h >> 8) % 40})"


def render_svg(stacks, title, unit="samples", min_pct=0.2, fill=hashed_fill, note=""):
    """Return a flamegraph SVG. `fill(path, name)` picks each frame's colour."""
    total = sum(n for _, n in stacks)
    if total <= 0:
        return ('<svg xmlns="http://www.w3.org/2000/svg" width="600" height="40" font-family="monospace">'
                f'<text x="10" y="25">{html.escape(title)}: no samples</text></svg>')
    tree = build_tree(stacks)
    width, row, minw = 1400, 16, total * min_pct / 100
    rects = []

    def walk(node, name, path, x, depth):
        if node["n"] < minw:
            return
        rects.append((name, path, x, depth, node["n"]))
        cx = x
        for cname, child in sorted(node["c"].items()):
            walk(child, cname, path + [cname], cx, depth + 1)
            cx += child["n"]

    walk(tree, "all", [], 0.0, 0)
    maxd = max(d for _, _, _, d, _ in rects) + 1
    height = maxd * row + 60
    scale = (width - 20) / total
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="monospace" font-size="11">',
         '<rect width="100%" height="100%" fill="#fbfbf6"/>',
         f'<text x="{width / 2}" y="20" text-anchor="middle" font-size="15">{html.escape(title)}</text>',
         f'<text x="10" y="40" font-size="11">total {total:,.0f} {unit}; frames under {min_pct}% hidden; hover for detail{html.escape(note)}</text>']
    for name, path, x, d, n in rects:
        y = height - 10 - (d + 1) * row
        w = n * scale
        label = html.escape(name)
        o.append(f'<g><title>{label} ({n:,.0f} {unit}, {100 * n / total:.2f}%)</title>'
                 f'<rect x="{10 + x * scale:.1f}" y="{y}" width="{max(w - 0.5, 0.1):.1f}" height="{row - 1}" fill="{fill(";".join(path), name)}" rx="1"/>')
        chars = int(w / 6.6)
        if chars >= 3:
            txt = name if len(name) <= chars else name[: chars - 2] + ".."
            o.append(f'<text x="{12 + x * scale:.1f}" y="{y + row - 4}">{html.escape(txt)}</text>')
        o.append("</g>")
    o.append("</svg>")
    return "\n".join(o)


def diff_fill(before, after):
    """Colour a frame by how its share moved from `before` to `after`: red grew, blue shrank."""
    def fill(path, name):
        if not path:
            return "rgb(220,220,220)"
        delta = after.get(path, 0.0) - before.get(path, 0.0)
        # A 5-point move saturates the colour.
        strength = min(abs(delta) / 0.05, 1.0)
        fade = int(230 - 150 * strength)
        return f"rgb(230,{fade},{fade})" if delta > 0 else f"rgb({fade},{fade},230)"
    return fill


def svg(args):
    print(render_svg(load(args.input), args.title, args.unit, args.min_pct))


def diff(args):
    before, after = load(args.before), load(args.after)
    fill = diff_fill(path_shares(before), path_shares(after))
    print(render_svg(after, args.title, args.unit, args.min_pct, fill,
                     "; red: a bigger share than before, blue: smaller"))


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("fold-cpu"); a.add_argument("input"); a.add_argument("--tgid", type=int, default=0)
    a = sub.add_parser("fold-off"); a.add_argument("input"); a.add_argument("--tgid", type=int, default=0)
    a.add_argument("--comm", default="")
    a = sub.add_parser("svg"); a.add_argument("input"); a.add_argument("--title", default="flamegraph")
    a.add_argument("--unit", default="samples"); a.add_argument("--min-pct", type=float, default=0.2)
    a = sub.add_parser("diff"); a.add_argument("before"); a.add_argument("after")
    a.add_argument("--title", default="differential flamegraph")
    a.add_argument("--unit", default="samples"); a.add_argument("--min-pct", type=float, default=0.2)
    a = sub.add_parser("top"); a.add_argument("input"); a.add_argument("--n", type=int, default=40)
    a = sub.add_parser("under"); a.add_argument("input"); a.add_argument("--frame", required=True)
    a.add_argument("--depth", type=int, default=2); a.add_argument("--n", type=int, default=30)
    a = sub.add_parser("timeline"); a.add_argument("input"); a.add_argument("--bucket", type=float, default=0.25)
    a.add_argument("--pattern", action="append", default=[])
    args = p.parse_args()
    {"timeline": timeline, "fold-cpu": fold_cpu, "fold-off": fold_off, "svg": svg, "diff": diff,
     "top": top, "under": under}[args.cmd](args)


if __name__ == "__main__":
    main()
