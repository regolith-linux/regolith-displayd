# Shared by the monitor-labels-* tests: a headless sway with two outputs
# (HEADLESS-2 at scale 2), a private session bus, screenshots of the label
# corner, and a D-Bus client that stays on the bus. Sourced, not run.

work=$(mktemp -d)
export HOME="$work/home" XDG_RUNTIME_DIR="$work/run"
mkdir -p "$HOME"
mkdir -m 700 "$XDG_RUNTIME_DIR"
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
unset WAYLAND_DISPLAY SWAYSOCK DISPLAY DBUS_SESSION_BUS_ADDRESS
pids=()

# regolith-displayd's labels.
DISPLAYD=org.gnome.Mutter.DisplayConfig
LABELS_OBJECT=/org/regolith/displayd/MonitorLabels
LABELS_IFACE=org.regolith.displayd.MonitorLabels

cleanup() {
    kill "${pids[@]}" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*"
    for log in "$work"/*.log; do
        echo "--- $log"
        tail -n 40 "$log"
    done
    exit 1
}

# retry <tries, 0.5 s apart> <command...>
retry() {
    local tries=$1
    shift
    for _ in $(seq "$tries"); do
        if "$@"; then return 0; fi
        sleep 0.5
    done
    return 1
}

# label <output>: the white pixels in the top left corner of <output>: their
# count, a fingerprint of their positions and the height they span.
label() {
    grim -o "$1" -t ppm "$work/shot.ppm"
    python3 - "$work/shot.ppm" <<'PY'
import hashlib, sys
data = open(sys.argv[1], "rb").read()
fields, pos = [], 0
while len(fields) < 4:
    while data[pos:pos + 1].isspace():
        pos += 1
    end = pos
    while not data[end:end + 1].isspace():
        end += 1
    fields.append(data[pos:end])
    pos = end
pos += 1
width, height = int(fields[1]), int(fields[2])
white = []
for y in range(min(height, 240)):
    for x in range(min(width, 240)):
        i = pos + (y * width + x) * 3
        if min(data[i:i + 3]) > 230:
            white.append((x, y))
span = max(y for x, y in white) - min(y for x, y in white) + 1 if white else 0
print(len(white), hashlib.sha1(repr(white).encode()).hexdigest()[:12], span)
PY
}

has_label() {
    [ "$(label "$1" | cut -d " " -f 1)" -gt 100 ]
}

has_no_label() {
    [ "$(label "$1" | cut -d " " -f 1)" = 0 ]
}

shape() {
    label "$1" | cut -d " " -f 2
}

owner() {
    gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
        --method org.freedesktop.DBus.GetNameOwner "$1" > /dev/null 2>&1
}

# A D-Bus client that stays on the bus, as the Displays panel and
# regolith-powerd do. Arguments: destination, object and interface of the
# label methods. Reads "show <connector>=<number>..." and "hide" lines.
cat > "$work/client.py" <<'PY'
import sys
from gi.repository import Gio, GLib
dest, obj, iface = sys.argv[1:4]
bus = Gio.bus_get_sync(Gio.BusType.SESSION)
for line in sys.stdin:
    words = line.split()
    if words[0] == "show":
        labels = {c: GLib.Variant("i", int(n)) for c, n in (w.split("=") for w in words[1:])}
        method, params = "ShowMonitorLabels", GLib.Variant("(a{sv})", (labels,))
    else:
        method, params = "HideMonitorLabels", None
    bus.call_sync(dest, obj, iface, method, params, None,
                  Gio.DBusCallFlags.NONE, 5000, None)
    print("done", flush=True)
PY

# client_start <destination> <object> <interface>
client_start() {
    coproc CLIENT { exec python3 -u "$work/client.py" "$@" 2>> "$work/client.log"; }
    client_pid=$CLIENT_PID
    pids+=("$client_pid")
}

client() {
    echo "$*" >&"${CLIENT[1]}"
    local reply
    read -r -t 10 reply <&"${CLIENT[0]}" || fail "the client got no answer to '$*'"
    [ "$reply" = done ] || fail "the client answered '$reply' to '$*'"
}

# client_stop: the client leaves the bus.
client_stop() {
    kill "$client_pid"
    wait "$client_pid" 2>/dev/null || true
}

# another <destination> <object> <interface>: another client, which leaves
# the bus at once, asks to hide the labels and to show 7 on HEADLESS-1.
another() {
    gdbus call --session --dest "$1" --object-path "$2" --method "$3.HideMonitorLabels" > /dev/null \
        || fail "HideMonitorLabels from another client failed"
    gdbus call --session --dest "$1" --object-path "$2" --method "$3.ShowMonitorLabels" \
        "{'HEADLESS-1': <7>}" > /dev/null || fail "ShowMonitorLabels from another client failed"
}

start_sway() {
    printf 'xwayland disable\noutput HEADLESS-1 mode 1600x1000\n' > "$work/sway.cfg"
    sway -c "$work/sway.cfg" > "$work/sway.log" 2>&1 &
    pids+=($!)
    retry 60 compgen -G "$XDG_RUNTIME_DIR/sway-ipc.*.sock" > /dev/null || fail "sway did not start"
    SWAYSOCK=$(compgen -G "$XDG_RUNTIME_DIR/sway-ipc.*.sock" | head -n 1)
    export SWAYSOCK
    retry 20 swaymsg -t get_outputs > /dev/null || fail "sway IPC not ready"
    WAYLAND_DISPLAY=$(compgen -G "$XDG_RUNTIME_DIR/wayland-*" | grep -v '\.lock$' | head -n 1)
    WAYLAND_DISPLAY=${WAYLAND_DISPLAY##*/}
    [ -n "$WAYLAND_DISPLAY" ] || fail "sway created no wayland socket"
    export WAYLAND_DISPLAY
    swaymsg -q create_output || fail "sway cannot create a headless output"
    retry 20 sh -c '[ "$(swaymsg -t get_outputs | grep -c "\"name\": \"HEADLESS-")" = 2 ]' || fail "no second headless output"
    # The second output at scale 2: its label must be drawn at twice the size.
    swaymsg -q output HEADLESS-2 mode 1280x800 scale 2
    if ! grim -o HEADLESS-1 -t ppm "$work/probe.ppm" 2> "$work/grim.log"; then
        echo "SKIP: grim cannot take a screenshot of the headless output here: $(cat "$work/grim.log")"
        exit 77
    fi
    has_no_label HEADLESS-1 || fail "white pixels on HEADLESS-1 before any label was shown"
}

start_bus() {
    dbus-daemon --session --nofork --print-address=1 > "$work/bus" 2> "$work/dbus.log" &
    pids+=($!)
    retry 20 test -s "$work/bus" || fail "dbus-daemon did not start"
    DBUS_SESSION_BUS_ADDRESS=$(head -n 1 "$work/bus")
    export DBUS_SESSION_BUS_ADDRESS
}

# start_displayd: sets $displayd.
start_displayd() {
    RUST_LOG=info regolith-displayd >> "$work/displayd.log" 2>&1 &
    displayd=$!
    pids+=("$displayd")
    retry 40 owner "$DISPLAYD" || fail "regolith-displayd does not own $DISPLAYD"
}
