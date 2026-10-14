#!/usr/bin/env python3
"""Every packaging channel states the same file capabilities, before anything is built.

packaging/linux/capabilities.txt is the matrix. Each channel grants capabilities in its own
recipe (a deb postinst, an rpm `%caps`, a pacman scriptlet, a NixOS wrapper, the Steam Deck
installer, the sysext builders). This reads each recipe and fails when a channel grants what the
matrix does not, or omits what it does. A grant it cannot attribute to a known binary fails too.
scripts/ci/assert-cap-matrix.sh checks the host and worker rows on the built package.

Usage: python3 scripts/ci/check-packaging-policy.py [--self-test]
"""
import glob
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
MATRIX = "packaging/linux/capabilities.txt"
GATE = "scripts/ci/assert-cap-matrix.sh"
HOST = "punktfunk-host"
WORKER = "punktfunk-encode-worker"
GAMESCOPE = "punktfunk-gamescope"


def strip_comments(text):
    """Drop `#` comments (shell, spec, nix); `$#` and `${#x}` have no space before the `#`."""
    return "\n".join(re.sub(r"(^|\s)#.*$", "", line) for line in text.splitlines())


def caps_norm(caps):
    """`CAP_SYS_NICE+ep` and `cap_sys_nice=ep` are one capability; none is `-`."""
    caps = caps.strip().lower().replace("+", "=")
    return "-" if caps in ("", "-", "(none)", "<none>") else caps


def binary_of(target, variables):
    """The punktfunk binary a setcap target names, by literal path or by shell variable."""
    target = target.strip("'\"")
    for var, binary in variables.items():
        if re.search(r"\$\{?%s\b" % var, target):
            return binary
    return os.path.basename(target)


# `setcap` in command position: after a line start, `sudo`, `;`, `&&`, `then`, not inside prose.
SETCAP = re.compile(r"""(?:^|[;&|({]|\bsudo|\bthen|\bdo|\belse)\s*setcap\s+(?!-)"""
                    r"""(?:'([^']*)'|"([^"]*)"|(\S+))\s+(\S+)""", re.M)


def read_setcap(text, variables):
    """[(binary, caps)] for each `setcap <caps> <path>`; `setcap -r` grants nothing."""
    return [(binary_of(m.group(4), variables), caps_norm(m.group(1) or m.group(2) or m.group(3)))
            for m in SETCAP.finditer(strip_comments(text))]


def read_rpm(text, variables):
    """[(binary, caps)] for each `%caps(<caps>) <path>` line of a spec."""
    return [(binary_of(m.group(2), variables), caps_norm(m.group(1)))
            for m in re.finditer(r"^%caps\(([^)]*)\)\s+(\S+)", strip_comments(text), re.M)]


def read_nix(text, variables):
    """[(binary, caps)] for each `security.wrappers.<name> = { capabilities = "…"; }`."""
    out = []
    for m in re.finditer(r"security\.wrappers\.([\w-]+)\s*=\s*\{(.*?)\n\s*\};",
                         strip_comments(text), re.S):
        caps = re.search(r'capabilities\s*=\s*"([^"]*)"', m.group(2))
        out.append((m.group(1), caps_norm(caps.group(1) if caps else "")))
    return out


def read_seal(text, variables):
    """The sysext builders grant the worker through `pf_seal_caps`, which reads the gate."""
    if re.search(r"^\s*pf_seal_caps\s", strip_comments(text), re.M):
        return [(WORKER, variables["WANT_WORKER_CAPS"])]
    return []


# channel -> (binaries it ships or None for all, [(files, reader, shell variables naming a binary)]).
# A glob reads every recipe of the channel, so a setcap added to a sibling script is seen.
CHANNELS = {
    "deb": (None, [(["packaging/debian/*.sh"], read_setcap, {})]),
    "rpm": (None, [(["packaging/rpm/*.spec", "packaging/gamescope/*.spec"], read_rpm, {})]),
    "arch": (None, [(["packaging/arch/PKGBUILD", "packaging/arch/*.install",
                      "packaging/gamescope/PKGBUILD", "packaging/gamescope/*.install"],
                     read_setcap, {})]),
    "nix": (None, [(["packaging/nix/*.nix"], read_nix, {})]),
    "steamdeck": (None, [(["scripts/steamdeck/*.sh"], read_setcap,
                          {"BIN": HOST, "WORKER": WORKER, "GS_BIN": GAMESCOPE})]),
    "source": ([GAMESCOPE], [(["packaging/gamescope/build-punktfunk-gamescope.sh"], read_setcap,
                              {"DEST": GAMESCOPE})]),
    "sysext-arch": (None, [(["packaging/arch/build-sysext.sh"], read_seal, {})]),
    "sysext-bazzite": (None, [(["packaging/bazzite/build-sysext.sh"], read_seal, {})]),
}


def read_matrix(text):
    """{binary: (caps, channels or None)}"""
    rows = {}
    for line in text.splitlines():
        fields = line.split("#")[0].split()
        if fields:
            rows[fields[0]] = (caps_norm(fields[1]), fields[2:] or None)
    return rows


def expected(matrix, binary, channel):
    caps, channels = matrix[binary]
    return caps if channels is None or channel in channels else "-"


def compare(matrix, channel, grants, covers=None):
    """Errors for one channel's [(binary, caps)] against the matrix rows it covers."""
    errors = [f"{channel} grants {caps} to {binary}, which {MATRIX} has no row for"
              for binary, caps in grants if binary not in matrix]
    for binary in covers or matrix:
        got = {caps for b, caps in grants if b == binary} or {"-"}
        want = expected(matrix, binary, channel)
        if got != {want}:
            errors.append(f"{channel} grants {binary} {' and '.join(sorted(got))}; "
                          f"{MATRIX} says {want}")
    return errors


def gate_grants(text):
    """The gate's constants as the host and worker rows the matrix must hold."""
    const = {m.group(1): m.group(2)
             for m in re.finditer(r"^(HOST_REL|WORKER_REL|WANT_WORKER_CAPS)='([^']*)'", text, re.M)}
    return const


def gate_errors(matrix, const):
    errors = []
    for key, binary in (("HOST_REL", HOST), ("WORKER_REL", WORKER)):
        if os.path.basename(const.get(key, "")) != binary:
            errors.append(f"{GATE} {key} is {const.get(key)!r}, not .../{binary}")
    want = caps_norm(const.get("WANT_WORKER_CAPS", ""))
    if want != matrix[WORKER][0]:
        errors.append(f"{GATE} WANT_WORKER_CAPS is {want}; {MATRIX} says {matrix[WORKER][0]}")
    if matrix[HOST][0] != "-":
        errors.append(f"{MATRIX} gives {HOST} {matrix[HOST][0]}; {GATE} fails any host capability")
    return errors


def read(path):
    with open(os.path.join(ROOT, path), encoding="utf-8") as f:
        return f.read()


def tree_errors():
    matrix = read_matrix(read(MATRIX))
    const = gate_grants(read(GATE))
    variables = {"WANT_WORKER_CAPS": caps_norm(const.get("WANT_WORKER_CAPS", ""))}
    errors = [(GATE, e) for e in gate_errors(matrix, const)]
    channels = {c for _, chans in matrix.values() if chans for c in chans}
    errors += [(MATRIX, f"channel {c} is not one of {sorted(CHANNELS)}")
               for c in sorted(channels - CHANNELS.keys())]
    for channel, (covers, sources) in CHANNELS.items():
        grants = []
        for patterns, reader, names in sources:
            files = sorted(f for p in patterns
                           for f in glob.glob(os.path.join(ROOT, p)))
            if not files:
                errors.append((patterns[0], f"{channel} has no recipe to read"))
            for f in files:
                with open(f, encoding="utf-8") as fh:
                    grants += reader(fh.read(), {**variables, **names})
        errors += [(patterns[0], e) for e in compare(matrix, channel, grants, covers)]
    return errors


def check():
    errors = tree_errors()
    for path, message in errors:
        print(f"::error file={path}::{message}")
    if errors:
        print(f"{len(errors)} packaging policy mismatch(es): make the channel and {MATRIX} agree.",
              file=sys.stderr)
        return 1
    print(f"check-packaging-policy: {len(CHANNELS)} channels match {MATRIX}")
    return 0


def self_test():
    fails = 0

    def expect(name, got, want):
        nonlocal fails
        if got != want:
            print(f"self-test {name}: got {got!r}, want {want!r}", file=sys.stderr)
            fails += 1

    ep, eip = "cap_sys_nice=ep", "cap_sys_nice=eip"
    expect("caps spellings", [caps_norm(c) for c in ("CAP_SYS_NICE+ep", ep, "(none)", "")],
           [ep, ep, "-", "-"])
    sh = """\
# setcap 'cap_sys_nice=ep' /usr/bin/punktfunk-host   <- a comment
setcap -r /usr/bin/punktfunk-host 2>/dev/null || true
  setcap 'cap_sys_nice=ep' usr/bin/punktfunk-encode-worker 2>/dev/null || true
sudo setcap "CAP_SYS_NICE=eip" "$GS_BIN" || warn x
sudo setcap cap_sys_nice=ep ${WORKER}
"""
    expect("setcap", read_setcap(sh, {"GS_BIN": GAMESCOPE, "WORKER": WORKER}),
           [(WORKER, ep), (GAMESCOPE, eip), (WORKER, ep)])
    spec = """\
# %caps(cap_sys_nice=ep) %{_bindir}/punktfunk-host
# Declared with %%caps rather than a %%post setcap
%{_bindir}/punktfunk-host
%caps(cap_sys_nice=ep) %{_bindir}/punktfunk-encode-worker
"""
    expect("rpm", read_rpm(spec, {}), [(WORKER, ep)])
    nix = """\
      # security.wrappers.punktfunk-host = { capabilities = "cap_sys_nice=ep"; };
      security.wrappers.punktfunk-encode-worker = {
        source = "${cfg.host.package}/bin/punktfunk-encode-worker";
        capabilities = "cap_sys_nice=ep";
        owner = "root";
      };
"""
    expect("nix", read_nix(nix, {}), [(WORKER, ep)])
    expect("seal", read_seal("# pf_seal_caps x\npf_seal_caps \"$STAGE\" why\n", {"WANT_WORKER_CAPS": ep}),
           [(WORKER, ep)])
    expect("seal absent", read_seal("# pf_seal_caps x\n", {"WANT_WORKER_CAPS": ep}), [])

    matrix = read_matrix(f"{HOST} -\n{WORKER} {ep}\n{GAMESCOPE} {eip} arch\n")
    ok = [(WORKER, ep)]
    expect("clean", compare(matrix, "deb", ok), [])
    expect("scoped row", compare(matrix, "arch", ok + [(GAMESCOPE, eip)]), [])
    expect("scoped row elsewhere", len(compare(matrix, "deb", ok + [(GAMESCOPE, eip)])), 1)
    expect("scoped row missing", len(compare(matrix, "arch", ok)), 1)
    expect("channel covering one binary", compare(matrix, "source", [(GAMESCOPE, "-")], [GAMESCOPE]), [])
    expect("host granted", len(compare(matrix, "rpm", ok + [(HOST, ep)])), 1)
    expect("worker changed in one channel", len(compare(matrix, "deb", [(WORKER, eip)])), 1)
    expect("worker grant removed", len(compare(matrix, "nix", [])), 1)
    expect("unknown binary", len(compare(matrix, "deb", ok + [("punktfunk-tray", ep)])), 1)
    expect("worker twice, two ways", len(compare(matrix, "deb", ok + [(WORKER, eip)])), 1)

    const = {"HOST_REL": "usr/bin/punktfunk-host", "WORKER_REL": "usr/bin/punktfunk-encode-worker",
             "WANT_WORKER_CAPS": ep}
    expect("gate agrees", gate_errors(matrix, const), [])
    expect("gate caps drift", len(gate_errors(matrix, {**const, "WANT_WORKER_CAPS": eip})), 1)
    expect("gate path drift", len(gate_errors(matrix, {**const, "HOST_REL": "usr/bin/x"})), 1)
    expect("matrix host capped", len(gate_errors(read_matrix(f"{HOST} {ep}\n{WORKER} {ep}\n"), const)), 1)
    expect("gate reader", gate_grants("HOST_REL='usr/bin/punktfunk-host'\nWANT_WORKER_CAPS='x'\n"),
           {"HOST_REL": "usr/bin/punktfunk-host", "WANT_WORKER_CAPS": "x"})
    if fails:
        return 1
    print("check-packaging-policy self-test ok")
    return 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if argv[1:]:
        print("usage: check-packaging-policy.py [--self-test]", file=sys.stderr)
        return 2
    return check()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
