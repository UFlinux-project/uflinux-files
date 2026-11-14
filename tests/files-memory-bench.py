#!/usr/bin/env python3
"""Measure what the file manager's processes actually hold, and write a report.

`docs/memory-budget.md` states the budgets; `scripts/files-memory-gate.py` fails a
build over them. This is the half in between: it starts the real binaries, does the
thing a user would do, and reads `/proc/<pid>/smaps_rollup`. It manufactures nothing —
a number here is a number that was read, and a scenario it could not run says so in
the report rather than going quiet.

Six scenarios, one per row of the budget table:

  provider        `uflinux-files-provider`, asked for its model a hundred times.
  cold-window     a window opened on a 2k-entry folder, measured before interaction.
  warm-window     the same window after the folder has been listed and settled.
  columns-5k      that window switched to Miller columns over 5k entries.
  search          `uflinux-searchd` over a large index, idle and then queried.
  desktop-model   the provider holding a 200-item desktop, geometry set and read back.

Each is measured in three independent sessions, because the gate takes a median and a
median of one is the sample that happened to be unlucky. Each session starts its own
processes: PSS measured against a process that answered a previous scenario is not the
same measurement, and the difference is exactly the leak the cycle counts exist to find.

Run it inside a Wayland session, or let it bring up its own — with no
`WAYLAND_DISPLAY` it starts a private bus and a headless `labwc`, the arrangement
`files-ui-smoke.sh` uses. Without `labwc` it still runs the scenarios that need no
display and marks the rest skipped, which the gate reads as "not evaluated" rather
than "passed".

    tests/files-memory-bench.py --output /tmp/report.json
    scripts/files-memory-gate.py --report /tmp/report.json

`--quick` shrinks the fixtures so the machinery can be checked in seconds; it is not a
measurement and the report says so.
"""

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# The bus names and object paths, repeated here rather than imported from the crates
# because this script has to run against an installed set of binaries, not a build tree.
# On both interfaces the bus name and the interface name are the same string, which is
# why one constant serves as `--dest` and as the prefix of the member.
FILES_NAME = "org.uflinux.FileManager"
FILES_OBJECT = "/org/uflinux/FileManager"
PROVIDER_NAME = "org.uflinux.FileManager.Desktop1"
PROVIDER_OBJECT = "/org/uflinux/FileManager/Desktop"
SEARCH_NAME = "org.uflinux.Search1"
SEARCH_OBJECT = "/org/uflinux/Search"

# Both daemons take one JSON string and answer with one, so every call here sends a body
# that names its own method — and the interface declares a single `s` argument, so the
# body goes on the command line as a bare string rather than wrapped in a tuple, which is
# how a reply is printed and a tempting thing to copy. The `Request` enums on both sides
# are `deny_unknown_fields`, so a body carrying a field the protocol never heard of is
# refused outright: an extra `id`, however reasonable it looks, would turn every call in
# this script into an error and every number into a skip.
MARKER = "UFLINUX_MEMORY_BENCH_BUS"


def log(message):
    print(message, flush=True)


class Process:
    """A child process, its pid, and the two things worth reading out of `/proc`.

    PSS rather than RSS: these processes share GTK, GLib and Mesa with the rest of the
    desktop, and charging each of them for a full copy of a shared mapping charges the
    machine several times over for the same pages. That is the accounting the budget
    table uses and the only fair one here.
    """

    def __init__(self, argv, env, log_path, name=""):
        self.name = name or Path(argv[0]).name
        self.log = open(log_path, "wb")  # noqa: SIM115
        self.proc = subprocess.Popen(  # noqa: S603
            argv,
            env=env,
            stdout=self.log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    @property
    def pid(self):
        return self.proc.pid

    def mem(self):
        """The PSS rollup, split in two, or None once the process is gone.

        `pss` is the number the budget table is written against. The other two are there
        because that number is not one thing: `anon` is memory this program allocated and
        nobody else can reclaim, and `file` is library text mapped from disk, which the
        kernel can drop and re-read and which every GTK program on the desktop already
        pays for once. A provider at 28 MiB of which 21 is `file` is a smaller process
        than one at 28 MiB private, and a report that cannot say which it measured invites
        the table to be read against the wrong one.
        """
        wanted = {"Pss:": "pss", "Pss_Anon:": "anon", "Pss_File:": "file"}
        reading = {"pss": None, "anon": None, "file": None}
        try:
            with open(f"/proc/{self.pid}/smaps_rollup", encoding="ascii") as handle:
                for line in handle:
                    key = line.split(":", 1)[0] + ":"
                    if key in wanted:
                        reading[wanted[key]] = int(line.split()[1]) * 1024
        except (OSError, ValueError):
            return None
        return reading if reading["pss"] is not None else None

    def fds(self):
        """How many descriptors are open.

        A descriptor opened per folder and never closed is invisible in a single PSS
        sample and fatal over a session, so it is counted at both ends of the cycle loop
        rather than once.
        """
        try:
            return len(os.listdir(f"/proc/{self.pid}/fd"))
        except OSError:
            return None

    def alive(self):
        return self.proc.poll() is None

    def stop(self):
        if self.alive():
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        self.log.close()


def bus_call(dest, obj, method, body=None):
    """One D-Bus call, returning the text it printed. Errors come back as text too.

    `method` is the fully-qualified member name `gdbus` asks for, interface and member
    joined. For these two daemons that starts with the bus name, because each interface
    is named after it — but it does not for `org.freedesktop.DBus.ListNames` or
    `org.gtk.Actions.DescribeAll`, whose interfaces belong to somebody else, so the
    destination cannot simply be prefixed. Getting that wrong is silent: `gdbus` reports
    an error, every call is refused, and every scenario is skipped.

    Nothing raises: a scenario that cannot reach the bus has to be able to say so in the
    report, and a stack trace out of a measurement script is a report that got thrown
    away. A call with no argument must not be given one — `gdbus` refuses the extra
    parameter rather than ignoring it, which is how a no-argument `DescribeAll` turns
    into a window that never appeared.
    """
    argv = ["gdbus", "call", "--session", "--dest", dest, "--object-path", obj,
            "--method", method]
    if body is not None:
        argv.append(f'"{body}"')
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=60,  # noqa: S603
                              check=False)
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return done.stdout + done.stderr


def provider_call(member, body):
    """A call on the desktop-icons interface, whose name is the provider's bus name."""
    return bus_call(PROVIDER_NAME, PROVIDER_OBJECT, f"{PROVIDER_NAME}.{member}", body)


def search_call(member, body):
    """A call on the indexer's interface, named after its bus name for the same reason."""
    return bus_call(SEARCH_NAME, SEARCH_OBJECT, f"{SEARCH_NAME}.{member}", body)


def activate(action, target=None):
    """Fire a window action, the way a click or an accelerator would.

    The window exports its actions on the session bus, which is how a scenario drives the
    interface without a display server, a pointer, or a screen-reader library. The target
    is a GVariant inside an array, so a string target is quoted twice over: once as a
    GVariant string, once as a shell argument.
    """
    variant = f"[<{target}>]" if target else "[]"
    argv = ["gdbus", "call", "--session", "--dest", FILES_NAME,
            "--object-path", f"{FILES_OBJECT}/window/1",
            "--method", "org.gtk.Actions.Activate", f'"{action}"', variant, "[]"]
    subprocess.run(argv, capture_output=True, text=True, timeout=60, check=False)  # noqa: S603


def bus_has(name):
    out = bus_call("org.freedesktop.DBus", "/org/freedesktop/DBus",
                   "org.freedesktop.DBus.ListNames")
    return f"'{name}'" in out


def window_up():
    """Whether a window's action group has reached the bus.

    A proxy for "the window is on screen", and labelled as one in the report: the object
    appears when the application registers a window, which is close to a first frame but
    is not one. GTK's frame clock is the only honest measurement of a frame, and reading
    it needs the program itself to report it — a change to the program, not to this
    script. `DescribeAll` takes no arguments and answers with every action the window has;
    `back` is in all of them, and an error reply is not.
    """
    out = bus_call(FILES_NAME, f"{FILES_OBJECT}/window/1", "org.gtk.Actions.DescribeAll")
    return "back" in out and "GDBus.Error" not in out


def wait_for(predicate, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.1)
    return False


def make_tree(root, dirs, per_dir):
    """Create `dirs` directories of `per_dir` files.

    Batched through one `touch` per five hundred names, because 200k process launches
    would cost more than the thing being measured, and would show up in the machine's own
    memory pressure while they ran.
    """
    root = Path(root)
    for index in range(dirs):
        folder = root / f"d{index}"
        folder.mkdir(parents=True, exist_ok=True)
        for start in range(0, per_dir, 500):
            names = [str(folder / f"f{i}.txt") for i in range(start, min(start + 500, per_dir))]
            subprocess.run(["touch", *names], check=False, capture_output=True)  # noqa: S603
    return root


def skipped(reason):
    return {"skipped": reason}


def moments(readings):
    """The PSS of each moment, plus the private/shared half of each that was recorded."""
    out = {}
    for moment, reading in readings:
        out[f"{moment}_pss_bytes"] = None if reading is None else reading["pss"]
        if reading is not None:
            out[f"{moment}_anon_bytes"] = reading["anon"]
            out[f"{moment}_file_bytes"] = reading["file"]
    return out


def measure(proc, settled, exercised, warmed, fds_before, cycles):
    """Sample a process that is still running. Gone means no measurement, not zero."""
    cycled = proc.mem()
    after = proc.fds()
    if cycled is None:
        return skipped("the process exited before the cycles finished")
    result = moments([("settled", settled), ("exercised", exercised),
                      ("warmed", warmed), ("cycled", cycled)])
    result["fds_before"] = fds_before
    result["fds_after"] = after
    result["cycles"] = cycles
    return result


def sample(settled, exercised, fds, cycles=0, **extra):
    """A scenario measured at one moment, where cycling was not the point."""
    out = moments([("settled", settled), ("exercised", exercised),
                   ("warmed", exercised), ("cycled", exercised)])
    out["fds_before"] = fds
    out["fds_after"] = fds
    out["cycles"] = cycles
    out.update(extra)
    return out


def fmt(value):
    return "-" if value is None else f"{value / 1024 / 1024:.1f} MiB"


class Bench:
    """The scenarios, over a private home, runtime directory and Desktop."""

    CYCLES = 100

    def __init__(self, root, binary, provider, searchd, quick):
        self.root = Path(root)
        self.binary = binary
        self.provider_binary = provider
        self.searchd = searchd
        self.quick = quick
        self.cycles = 10 if quick else self.CYCLES
        self.home = self.root / "home"
        self.desktop = self.home / "Desktop"
        self.config = self.home / ".config" / "uflinux-files"
        for path in (self.home, self.root / "xdg", self.desktop, self.config,
                     self.home / ".local" / "share", self.home / ".cache"):
            path.mkdir(parents=True, exist_ok=True)
        (self.root / "xdg").chmod(0o700)

    def env(self):
        """A private everything, and no accessibility bridge.

        `NO_AT_BRIDGE` because an a11y tree built for a probe that is not running is
        memory the scenario did not ask for and the budget table makes no room for. The
        UI smoke test switches it off for the opposite reason — it is the only thing in
        the tree that reads the tree — and both are right for what they measure.
        """
        base = dict(os.environ)
        base.update({
            "HOME": str(self.home),
            "XDG_RUNTIME_DIR": str(self.root / "xdg"),
            "XDG_DATA_HOME": str(self.home / ".local" / "share"),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_CACHE_HOME": str(self.home / ".cache"),
            "XDG_DESKTOP_DIR": str(self.desktop),
            "NO_AT_BRIDGE": "1",
        })
        return base

    # --- the scenarios ---------------------------------------------------------------

    def provider(self):
        """The desktop-icons provider, asked for its model a hundred times.

        The tightest number on the table, and the one that decides whether the desktop
        icons feature costs anything: this process is resident for the whole session on
        every machine that has a desktop at all.
        """
        (self.desktop / "note.txt").write_text("x")
        if not Path(self.provider_binary).exists():
            return skipped("no uflinux-files-provider to measure")
        proc = Process([self.provider_binary], self.env(),
                       self.root / "provider.log", "provider")
        if not wait_for(lambda: bus_has(PROVIDER_NAME), 20):
            proc.stop()
            return skipped("the provider never took its bus name")
        settled = proc.mem()
        before = proc.fds()
        provider_call("GetModel", '{"method":"get-model"}')
        exercised = proc.mem()
        warmed = proc.mem()
        for _ in range(self.cycles):
            provider_call("GetModel", '{"method":"get-model"}')
        result = measure(proc, settled, exercised, warmed, before, self.cycles)
        proc.stop()
        return result

    def window(self):
        """One window on a 2k-entry folder, measured at two moments.

        Both numbers come from one process, and the process is handed back still running:
        the budget table's two rows are the same window before and after its folder is
        listed, and starting a second one would measure startup twice and the listing
        once. The caller owns the process and has to stop it.
        """
        folder = make_tree(self.root / "two-k", 1, 200 if self.quick else 2000)
        started = time.monotonic()
        proc = Process([self.binary, folder.as_uri()], self.env(),
                       self.root / "window.log", "window")
        if not wait_for(window_up, 30):
            proc.stop()
            return None, skipped("no window appeared"), skipped("no window appeared")
        cold_ms = int((time.monotonic() - started) * 1000)
        settled = proc.mem()
        # The listing has arrived when the status line has something to say. Reading that
        # needs the accessibility tree, which this run switches off, so the settle is a
        # fixed wait instead: long enough for a first pass over 2k entries, and honest
        # about being a wait rather than an observation.
        time.sleep(3.0)
        warmed = proc.mem()
        fds = proc.fds()
        return (proc,
                sample(settled, settled, fds, first_window_ms=cold_ms),
                sample(settled, warmed, fds))

    def columns(self, proc):
        """The window already open, switched to Miller columns over 5k entries.

        Cycling between two view modes is the repetition a person actually performs —
        nobody opens five thousand folders by hand — and it is the one that would show a
        widget tree being rebuilt rather than reused.
        """
        folder = make_tree(self.root / "five-k", 1, 300 if self.quick else 5000)
        # Hand the folder to the running instance over the bus rather than synthesising a
        # click: a second invocation takes the same path a launcher does.
        subprocess.run([self.binary, folder.as_uri()], env=self.env(),
                       capture_output=True, check=False)  # noqa: S603
        time.sleep(3.0)
        settled = proc.mem()
        activate("mode", "'columns'")
        time.sleep(3.0)
        exercised = proc.mem()
        warmed = proc.mem()
        before = proc.fds()
        rounds = max(1, self.cycles // 10)
        for _ in range(rounds):
            activate("mode", "'list'")
            time.sleep(0.2)
            activate("mode", "'columns'")
            time.sleep(0.2)
        return measure(proc, settled, exercised, warmed, before, rounds)

    def search(self):
        """The indexer over a real index, idle and then queried.

        The idle number is the one on the table and the one a user pays all session; the
        queried number is the peak a search is allowed to cause. Both come from the same
        process, because the question worth asking is what a query adds.
        """
        if not Path(self.searchd).exists():
            return skipped("no uflinux-searchd to measure"), skipped("no uflinux-searchd")
        docs = 2_000 if self.quick else 200_000
        tree = make_tree(self.root / "index", docs // 1000, 1000)
        # `version` is not optional: the loader rejects a file claiming any other, falls
        # back to the defaults, and would then index $HOME instead of the fixture — a
        # crawl of a home directory that happens to contain nothing, producing a number
        # that looks like a small index while measuring the wrong directory.
        (self.config / "index.json").write_text(json.dumps(
            {"version": 1, "roots": [{"path": str(tree), "content": "off"}]}))
        proc = Process([self.searchd], self.env(), self.root / "searchd.log", "searchd")
        if not wait_for(lambda: bus_has(SEARCH_NAME), 20):
            proc.stop()
            return skipped("the indexer never took its bus name"), skipped("no bus name")

        def records():
            """How many documents the daemon says it has, or None while it is writing."""
            done = subprocess.run([self.searchd, "--status"], env=self.env(),
                                  capture_output=True, text=True, timeout=30,  # noqa: S603
                                  check=False)
            try:
                status = json.loads(done.stdout)
            except (json.JSONDecodeError, ValueError):
                return None
            if status.get("busy"):
                return None
            return max((index.get("records", 0) for index in status.get("indexes", [])),
                       default=0)

        if not wait_for(lambda: (records() or 0) > 0, 900):
            proc.stop()
            return skipped("the index never filled in"), skipped("the index never filled in")
        idle = proc.mem()
        before = proc.fds()
        search_call("SearchFiles", '{"method":"search-files","text":"f0"}')
        queried = proc.mem()
        warmed = proc.mem()
        for _ in range(self.cycles):
            search_call("SearchFiles", '{"method":"search-files","text":"f1"}')
        idle_result = sample(idle, idle, before, indexed_documents=docs,
                             fds_after=proc.fds())
        query = measure(proc, idle, queried, warmed, before, self.cycles)
        if not query.get("skipped"):
            query["indexed_documents"] = docs
        proc.stop()
        return idle_result, query

    def desktop_model(self):
        """The provider with a full desktop: 200 items, geometry set, model read back.

        The 12 MiB in the table is what the renderer — `uflinux-desktop`, a different
        program — is allowed for drawing those icons. What is measurable here is the
        model that renderer would be handed, which is the part this program pays for.
        """
        for index in range(20 if self.quick else 200):
            (self.desktop / f"item{index}.txt").write_text("x")
        proc = Process([self.provider_binary], self.env(),
                       self.root / "desktop.log", "desktop-provider")
        if not wait_for(lambda: bus_has(PROVIDER_NAME), 20):
            proc.stop()
            return skipped("the provider never took its bus name")
        settled = proc.mem()
        before = proc.fds()
        # `monitors` is an array, because a grid that stopped being mentioned is a
        # monitor that was unplugged; the cell fields are `cell_w` and `cell_h`, and the
        # request refuses a field it does not know.
        provider_call("SetGeometry",
                      '{"method":"set-geometry","monitors":[{"monitor":0,"columns":10,'
                      '"rows":20,"cell_w":96,"cell_h":96}]}')
        exercised = proc.mem()
        warmed = proc.mem()
        for _ in range(self.cycles):
            provider_call("GetModel", '{"method":"get-model"}')
        result = measure(proc, settled, exercised, warmed, before, self.cycles)
        if not result.get("skipped"):
            result["items"] = len(list(self.desktop.iterdir()))
        proc.stop()
        return result

    # --- running them -----------------------------------------------------------------

    def session(self, index):
        """One independent pass over every scenario. Fresh processes, fresh numbers."""
        out = {"provider": self.provider()}
        proc, cold, warm = self.window()
        out["cold-window"] = cold
        out["warm-window"] = warm
        out["columns-5k"] = self.columns(proc) if proc else skipped("no window to switch")
        if proc is not None:
            proc.stop()
        idle, queried = self.search()
        out["search"] = idle
        out["search-query"] = queried
        out["desktop-model"] = self.desktop_model()
        log(f"session {index}: " + "  ".join(
            f"{name}={'skipped' if r.get('skipped') else fmt(r.get('warmed_pss_bytes'))}"
            for name, r in out.items()))
        return out


def has_render_node(dri):
    """Whether a machine has a GPU, by the same test the program applies.

    Kept identical to `ui::has_render_node` on purpose: the report says a machine has no
    GPU only when the program would also have decided that, so a reader can tell whether
    the window numbers were taken with cairo or with GL.
    """
    try:
        return any(entry.name.startswith(("card", "renderD")) for entry in dri.iterdir())
    except OSError:
        return False


def environment(compositor, renderer, note):
    available = 0
    try:
        with open("/proc/meminfo", encoding="ascii") as handle:
            for line in handle:
                if line.startswith("MemAvailable:"):
                    available = int(line.split()[1]) * 1024
    except OSError:
        pass
    return {
        "renderer": renderer,
        "compositor": compositor,
        "scale": os.environ.get("GDK_SCALE", "1"),
        "machine": f"{platform.machine()} {platform.system()} {platform.release()}",
        "available_memory_bytes": available,
        # Whether there is a render node decides what GTK's GL renderer costs, and the
        # renderer the program picked follows from it. Without one, Mesa answers with
        # llvmpipe and allocates its rasteriser in anonymous memory, which measured as the
        # largest single number in a window's PSS on the class of machine the budget
        # targets. A report that does not say which of the two it ran on cannot be read
        # against the table at all.
        "dri": has_render_node(Path("/dev/dri")),
        "gsk_renderer": os.environ.get("GSK_RENDERER")
        or ("gtk-default" if has_render_node(Path("/dev/dri")) else "cairo"),
        "note": note,
    }


def compositor_version():
    try:
        done = subprocess.run(["labwc", "--version"], capture_output=True, text=True,  # noqa: S603
                              timeout=10, check=False)
    except (OSError, subprocess.TimeoutExpired):
        return "labwc"
    lines = (done.stdout + done.stderr).strip().splitlines()
    return lines[0] if lines else "labwc"


def own_bus():
    """Re-exec under a private session bus, once, and never twice.

    A measurement taken against someone's running desktop is a measurement of their
    desktop too, and a provider attached to a real session bus leaves a name claimed
    behind when the script is interrupted. The marker stops a loop in case the bus never
    arrives.
    """
    if os.environ.get(MARKER) or not shutil.which("dbus-run-session"):
        return
    os.environ[MARKER] = "1"
    os.execvpe("dbus-run-session", ["dbus-run-session", "--", sys.executable, *sys.argv],
               os.environ)


def own_compositor(root):
    """A headless `labwc` on a private display, or None if it will not come up.

    Its output goes to a file rather than to the bit bucket: a compositor that refuses to
    start takes the four window scenarios with it, and a report that says only "no
    compositor" leaves the reader to guess whether that means an absent package or a
    broken one.
    """
    if not shutil.which("labwc"):
        return None
    env = dict(os.environ)
    env["XDG_RUNTIME_DIR"] = str(root / "xdg")
    env["WLR_BACKENDS"] = "headless"
    env["WLR_RENDERER"] = "pixman"
    env["WAYLAND_DISPLAY"] = "wayland-0"
    # The runtime directory has to exist and be 0700 before the compositor starts: it
    # creates the socket there and refuses to create the directory itself, which reads as
    # a compositor that starts and then does not answer.
    (root / "xdg").mkdir(exist_ok=True)
    (root / "xdg").chmod(0o700)
    with open(root / "labwc.log", "wb") as log_file:
        lab = subprocess.Popen(["labwc"], env=env,  # noqa: S603
                               stdout=log_file, stderr=subprocess.STDOUT,
                               start_new_session=True)
        if not wait_for(lambda: (Path(env["XDG_RUNTIME_DIR"]) / "wayland-0").exists(), 20):
            lab.terminate()
            return None
    os.environ.update(env)
    return lab


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", default=os.environ.get("UFLINUX_FILES_BIN",
                                                           "target/debug/uflinux-files"))
    parser.add_argument("--provider-binary",
                        default=os.environ.get("UFLINUX_PROVIDER_BIN", ""))
    parser.add_argument("--searchd", default=os.environ.get("UFLINUX_SEARCHD_BIN", ""))
    parser.add_argument("--output", type=Path, default=Path("memory-report.json"))
    parser.add_argument("--sessions", type=int, default=3)
    parser.add_argument("--quick", action="store_true",
                        help="tiny fixtures: checks the plumbing, not the budget")
    args = parser.parse_args()

    binary = Path(args.binary).resolve()
    if not binary.exists():
        log(f"no binary at {binary} — run cargo build -p uflinux-files -p uflinux-searchd")
        return 2
    searchd = args.searchd or str(binary.parent / "uflinux-searchd")
    provider = args.provider_binary or str(binary.parent / "uflinux-files-provider")

    had_display = bool(os.environ.get("WAYLAND_DISPLAY"))
    if not had_display:
        own_bus()
    root = Path(tempfile.mkdtemp(prefix="ufm-mem-"))
    lab = None if had_display else own_compositor(root)
    if lab:
        compositor = compositor_version()
        note = "headless labwc, pixman renderer, no GPU"
    elif had_display:
        compositor = "the running session"
        note = "measured against a running desktop"
    else:
        compositor = "none"
        note = ("no compositor: no WAYLAND_DISPLAY and labwc would not start — the four "
                "window scenarios need one")
    if args.quick:
        note += "; QUICK MODE — fixtures shrunk, not a budget measurement"

    bench = Bench(root, str(binary), provider, searchd, args.quick)
    sessions = [{"id": number, "scenarios": bench.session(number)}
                for number in range(1, max(1, args.sessions) + 1)]
    if lab is not None:
        lab.terminate()

    report = {"version": 1,
              "environment": environment(compositor,
                                         os.environ.get("WLR_RENDERER", "unknown"), note),
              "sessions": sessions}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    log(f"wrote {args.output}")
    shutil.rmtree(root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
