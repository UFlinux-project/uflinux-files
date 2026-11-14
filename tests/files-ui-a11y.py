#!/usr/bin/env python3
"""Read the accessibility tree of a running `uflinux-files`.

Used by `files-ui-smoke.sh`, which needs to know what a window actually exposes
rather than what the widget code appears to build. The tree is the interface a
screen reader sees, so it is the closest thing this program has to a public
contract for the interface, and it is what a test can assert on without
comparing pixels.

Two things about this are not obvious and are worth stating before the code:

The probe runs in its own process. Calling `Atspi.init()` and walking the tree
from inside the GTK process being inspected deadlocks — the walk asks the
process for its own tree over a bus that the process is itself blocked serving —
and `get_child_count` comes back as -1. A separate process is the only shape
that works.

Every accessor here is wrapped. AT-SPI answers with an error rather than an
exception-free result whenever a node went away between two calls, which happens
constantly while the file manager is repainting, and a test that dies because a
label was replaced mid-walk reports the wrong thing. A node that cannot be read
is reported as unreadable and the walk continues; the assertions in the shell
script are about what is present, so a missing line is a failure there rather
than a crash here.

Run directly to see the tree:

    tests/files-ui-a11y.py dump
"""

import sys

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

# The states the smoke test distinguishes between. A header button that is
# insensitive is how the program says "there is nowhere further to go back to",
# which is a fact worth asserting; `selected` is how a test confirms a row it
# asked to select actually took; the rest of the state set is noise here.
STATES = ("visible", "showing", "sensitive", "focusable", "selected")

# Stop rather than hang if the tree is deeper or wider than expected. A window
# with a folder of ten thousand entries would otherwise walk for minutes.
LIMIT = 20000


def name(acc):
    try:
        return (Atspi.Accessible.get_name(acc) or "").replace("\t", " ").strip()
    except Exception:  # noqa: BLE001
        return "<unreadable>"


def role(acc):
    try:
        return Atspi.Accessible.get_role_name(acc)
    except Exception:  # noqa: BLE001
        return "<unreadable>"


def states(acc):
    try:
        set_ = Atspi.Accessible.get_state_set(acc)
    except Exception:  # noqa: BLE001
        return ""
    return ",".join(s for s in STATES if set_.contains(getattr(Atspi.StateType, s.upper())))


def children(acc):
    """The children of a node, or none when the node cannot answer.

    `get_child_count` returns a negative number on error rather than failing, and
    passing that to a range would silently produce nothing.
    """
    try:
        count = Atspi.Accessible.get_child_count(acc)
    except Exception:  # noqa: BLE001
        return []
    if count < 0:
        return []
    out = []
    for i in range(count):
        try:
            child = Atspi.Accessible.get_child_at_index(acc, i)
        except Exception:  # noqa: BLE001
            continue
        if child is not None:
            out.append(child)
    return out


def apps():
    desktop = Atspi.get_desktop(0)
    try:
        count = Atspi.Accessible.get_child_count(desktop)
    except Exception:  # noqa: BLE001
        return []
    return [] if count < 0 else children(desktop)


seen = 0


def walk(acc, out):
    global seen  # noqa: PLW0603
    seen += 1
    if seen > LIMIT:
        return
    out.append("%s\t%s\t%s" % (role(acc), name(acc), states(acc)))
    for child in children(acc):
        walk(child, out)


def dump():
    lines = []
    for app in apps():
        walk(app, lines)
    print("\n".join(lines))
    return 0


def first_entry():
    """The first node with the `entry` role, which is the search field.

    Depth-first, so the answer is the same one every run: the search bar sits
    above the folder views in the widget tree, and it is the only editable field
    a window has until the connect dialog or a name prompt is open.
    """
    stack = list(apps())
    while stack:
        acc = stack.pop(0)
        if role(acc) == "entry":
            return acc
        stack = children(acc) + stack
    return None


def first_named(wanted):
    """The first node whose accessible name is WANTED, depth-first.

    Same order as `dump`, so a test that reads the tree and then presses
    something reaches the node it just saw.
    """
    stack = list(apps())
    while stack:
        acc = stack.pop(0)
        if name(acc) == wanted:
            return acc
        stack = children(acc) + stack
    return None


def type_into(text):
    acc = first_entry()
    if acc is None:
        print("no entry")
        return 2
    iface = Atspi.Accessible.get_editable_text_iface(acc)
    if iface is None:
        print("entry has no EditableText interface")
        return 3
    Atspi.EditableText.set_text_contents(iface, text)
    return 0


def read_entry():
    acc = first_entry()
    if acc is None:
        print("no entry")
        return 2
    iface = Atspi.Accessible.get_text_iface(acc)
    if iface is None:
        print("entry has no Text interface")
        return 3
    print(Atspi.Text.get_text(iface, 0, -1))
    return 0


def press(wanted):
    """Run the default action of the first node named WANTED.

    This is how a test clicks a dialog's button: the accessibility tree is what
    the script can see, and an activation through it goes through the same
    controller a mouse click would.
    """
    acc = first_named(wanted)
    if acc is None:
        print("no node named %s" % wanted)
        return 2
    iface = Atspi.Accessible.get_action_iface(acc)
    if iface is None:
        print("%s has no Action interface" % wanted)
        return 3
    if iface.get_n_actions() < 1:
        print("%s offers no action" % wanted)
        return 4
    if not iface.do_action(0):
        print("%s refused its action" % wanted)
        return 5
    return 0


def select(wanted):
    """Select the first selectable node named WANTED.

    This is how a test picks a row before asking the window to copy it. The
    selection lives on the container — a grid cell or a tree-table row answers
    `Action` and `Component` but not `Selection` — so the walk keeps the path it
    took to reach a node and hands that to `select_at`, which finds the row and
    the container that holds it. Breadth-first, so the shallowest node with the
    name wins: the row itself rather than the label inside it.
    """
    queue = [(app, []) for app in apps()]
    while queue:
        acc, trail = queue.pop(0)
        for position, child in enumerate(children(acc)):
            step = trail + [(acc, position)]
            if name(child) == wanted:
                answer = select_at(step)
                if answer is not None:
                    return answer
            queue.append((child, step))
    print("no node named %s" % wanted)
    return 2


def select_at(trail):
    """Select the node at the end of TRAIL, a list of (parent, index) pairs.

    The trail is walked from the node upwards, and the first container that
    answers `Selection` gets the call — which is the grid or the tree table
    rather than the cell, and the index is the row's position inside it. Returns
    `None` when nothing on the way up holds a selection, so a label in a banner
    that merely repeats a folder's name is skipped and the search continues; a
    container that answers but refuses is a real failure and is reported.
    """
    for parent, index in reversed(trail):
        iface = Atspi.Accessible.get_selection_iface(parent)
        if iface is None:
            continue
        if not Atspi.Selection.select_child(iface, index):
            print("selection refused at index %d" % index)
            return 5
        return 0
    print("no Selection interface above the node")
    return 3


def main(argv):
    Atspi.init()
    command = argv[0] if argv else "dump"
    if command == "dump":
        return dump()
    if command == "type":
        return type_into(argv[1])
    if command == "entry":
        return read_entry()
    if command == "press":
        return press(argv[1])
    if command == "select":
        return select(argv[1])
    print(
        "usage: files-ui-a11y.py [dump | type TEXT | entry | press NAME | select NAME]",
        file=sys.stderr,
    )
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
