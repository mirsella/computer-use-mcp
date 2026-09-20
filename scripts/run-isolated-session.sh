#!/usr/bin/env bash
# Run computer-use-mcp inside a fully private virtual Wayland session.
#
# This launcher owns every process in the session.  It deliberately starts the
# session bus, compositor, PipeWire, portal services, and AT-SPI services
# itself instead of allowing a private client to activate services through the
# ambient per-user service manager.
#
# No service is described as isolated until its private sockets and D-Bus names
# have been checked.  The readiness marker is consumed by src/session.rs and is
# removed with the private runtime directory during teardown.
set -euo pipefail

WIDTH=1920
HEIGHT=1080
SCALE=1
SOCKET_TIMEOUT_SECS="${COMPUTER_USE_MCP_STARTUP_TIMEOUT_SECS:-15}"
PORTAL_AUTH_MODE="${COMPUTER_USE_MCP_PRIVATE_PORTAL_AUTH:-auto}"

usage() {
    cat <<'EOF'
Usage: run-isolated-session.sh [--width N] [--height N] [--scale N] [--] [command...]

Launches a KDE virtual Wayland session with private D-Bus, PipeWire,
portals, and AT-SPI services, then runs the given command inside it.
The default command is: computer-use-mcp mcp
EOF
}

COMMAND=()
while [ "$#" -gt 0 ]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --width)
            WIDTH="${2:?--width needs a value}"
            shift 2
            ;;
        --height)
            HEIGHT="${2:?--height needs a value}"
            shift 2
            ;;
        --scale)
            SCALE="${2:?--scale needs a value}"
            shift 2
            ;;
        --)
            shift
            while [ "$#" -gt 0 ]; do
                COMMAND+=("$1")
                shift
            done
            break
            ;;
        --*)
            printf 'run-isolated-session.sh: unknown option: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
        *)
            COMMAND+=("$1")
            shift
            ;;
    esac
done

if [ "${#COMMAND[@]}" -eq 0 ]; then
    COMMAND=(computer-use-mcp mcp)
fi

for value in "$WIDTH" "$HEIGHT" "$SCALE" "$SOCKET_TIMEOUT_SECS"; do
    case "$value" in
        ''|*[!0-9]*)
            printf 'run-isolated-session.sh: dimensions and timeout must be positive integers\n' >&2
            exit 2
            ;;
    esac
done
if [ "$WIDTH" -le 0 ] || [ "$HEIGHT" -le 0 ] || [ "$SCALE" -le 0 ] || [ "$SOCKET_TIMEOUT_SECS" -le 0 ]; then
    printf 'run-isolated-session.sh: dimensions and timeout must be positive integers\n' >&2
    exit 2
fi

case "$PORTAL_AUTH_MODE" in
    auto|off|require)
        ;;
    *)
        printf 'run-isolated-session.sh: COMPUTER_USE_MCP_PRIVATE_PORTAL_AUTH must be auto, off, or require\n' >&2
        exit 2
        ;;
esac

if [[ -v COMPUTER_USE_MCP_BIN ]]; then
    if [ -z "$COMPUTER_USE_MCP_BIN" ]; then
        printf 'run-isolated-session.sh: COMPUTER_USE_MCP_BIN must not be empty\n' >&2
        exit 1
    fi
    # The override is the executable that will actually run, not merely a
    # binary checked while the command array continues to use another one.
    COMMAND[0]="$COMPUTER_USE_MCP_BIN"
fi

require_executable() {
    local value="$1"
    if [[ "$value" == */* ]]; then
        [ -x "$value" ] || return 1
    else
        command -v "$value" >/dev/null 2>&1 || return 1
    fi
}

resolve_binary() {
    local override_var="$1"
    local command_name="$2"
    shift 2
    local override="${!override_var-}"
    local candidate

    if [ -n "$override" ]; then
        require_executable "$override" || return 1
        printf '%s\n' "$override"
        return 0
    fi
    if command -v "$command_name" >/dev/null 2>&1; then
        command -v "$command_name"
        return 0
    fi
    for candidate in "$@"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

require_binary() {
    local output_var="$1"
    local override_var="$2"
    local command_name="$3"
    shift 3
    local resolved
    if ! resolved="$(resolve_binary "$override_var" "$command_name" "$@")"; then
        printf 'run-isolated-session.sh: required isolated-session dependency is unavailable: %s\n' "$command_name" >&2
        return 1
    fi
    printf -v "$output_var" '%s' "$resolved"
}

require_executable "${COMMAND[0]}" || {
    if [[ "${COMMAND[0]}" == */* ]]; then
        printf 'run-isolated-session.sh: server binary is not executable: %s\n' "${COMMAND[0]}" >&2
    else
        printf 'run-isolated-session.sh: server binary not found on PATH: %s\n' "${COMMAND[0]}" >&2
    fi
    exit 1
}

require_binary DBUS_DAEMON_BIN COMPUTER_USE_MCP_DBUS_DAEMON_BIN dbus-daemon /usr/bin/dbus-daemon
require_binary DBUS_SEND_BIN COMPUTER_USE_MCP_DBUS_SEND_BIN dbus-send /usr/bin/dbus-send
require_binary SETSID_BIN COMPUTER_USE_MCP_SETSID_BIN setsid /usr/bin/setsid
require_binary COMPOSITOR_BIN COMPUTER_USE_MCP_COMPOSITOR_BIN kwin_wayland /usr/bin/kwin_wayland
require_binary PIPEWIRE_BIN COMPUTER_USE_MCP_PIPEWIRE_BIN pipewire /usr/bin/pipewire
require_binary WIREPLUMBER_BIN COMPUTER_USE_MCP_WIREPLUMBER_BIN wireplumber /usr/bin/wireplumber
require_binary PORTAL_BIN COMPUTER_USE_MCP_PORTAL_BIN xdg-desktop-portal /usr/lib/xdg-desktop-portal /usr/libexec/xdg-desktop-portal
require_binary PORTAL_BACKEND_BIN COMPUTER_USE_MCP_PORTAL_BACKEND_BIN xdg-desktop-portal-kde /usr/lib/xdg-desktop-portal-kde /usr/libexec/xdg-desktop-portal-kde
require_binary ATSPI_BUS_BIN COMPUTER_USE_MCP_ATSPI_BUS_BIN at-spi-bus-launcher /usr/lib/at-spi-bus-launcher /usr/libexec/at-spi-bus-launcher
require_binary ATSPI_REGISTRY_BIN COMPUTER_USE_MCP_ATSPI_REGISTRY_BIN at-spi2-registryd /usr/lib/at-spi2-registryd /usr/libexec/at-spi2-registryd
GDBUS_BIN=""
if [ "$PORTAL_AUTH_MODE" = require ]; then
    require_binary GDBUS_BIN COMPUTER_USE_MCP_GDBUS_BIN gdbus /usr/bin/gdbus
elif [ "$PORTAL_AUTH_MODE" = auto ]; then
    if [[ -v COMPUTER_USE_MCP_GDBUS_BIN ]]; then
        require_binary GDBUS_BIN COMPUTER_USE_MCP_GDBUS_BIN gdbus /usr/bin/gdbus
    else
        GDBUS_BIN="$(resolve_binary COMPUTER_USE_MCP_GDBUS_BIN gdbus /usr/bin/gdbus || true)"
    fi
fi

RUNTIME_DIR="$(mktemp -d -t computer-use-mcp-isolated-XXXXXX)"
chmod 700 "$RUNTIME_DIR"
umask 077
ISOLATION_MARKER="$RUNTIME_DIR/isolation.ready"

declare -a SERVICE_LABELS=()
declare -a SERVICE_PIDS=()
declare -A SERVICE_PID_BY_LABEL=()
CLEANED_UP=0

process_has_isolation_marker() {
    local pid="$1"
    local entry
    local environment_fd
    if ! exec {environment_fd}<"/proc/$pid/environ" 2>/dev/null; then
        return 1
    fi
    while IFS= read -r -d '' entry; do
        if [ "$entry" = "COMPUTER_USE_MCP_ISOLATION_MARKER=$ISOLATION_MARKER" ]; then
            exec {environment_fd}<&-
            return 0
        fi
    done <&"$environment_fd"
    exec {environment_fd}<&-
    return 1
}

kill_marked_processes() {
    local signal="$1"
    local process_path
    local pid
    for process_path in /proc/[0-9]*; do
        [ -d "$process_path" ] || continue
        pid="${process_path##*/}"
        [ "$pid" = "$$" ] && continue
        if [[ -O "$process_path" ]] && process_has_isolation_marker "$pid"; then
            kill "-$signal" "$pid" 2>/dev/null || true
        fi
    done
}

cleanup() {
    local index
    local pid
    [ "$CLEANED_UP" -eq 1 ] && return
    CLEANED_UP=1

    # Every service was started by setsid, so each recorded PID is the leader
    # of a process group owned by this launcher.  Never use a broad process
    # name match: unrelated user processes must survive cleanup.
    for ((index=${#SERVICE_PIDS[@]} - 1; index >= 0; index--)); do
        pid="${SERVICE_PIDS[index]}"
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
        fi
    done
    # Applications may be launched through private D-Bus and detach from the
    # command process group.  The per-run marker path is the ownership key for
    # those descendants; never match by executable name or a reused PID.
    kill_marked_processes TERM
    sleep 0.1
    for ((index=${#SERVICE_PIDS[@]} - 1; index >= 0; index--)); do
        pid="${SERVICE_PIDS[index]}"
        if kill -0 "$pid" 2>/dev/null; then
            kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
        fi
    done
    kill_marked_processes KILL
    for pid in "${SERVICE_PIDS[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
    if [ -n "${RUNTIME_DIR:-}" ] && [ -d "$RUNTIME_DIR" ]; then
        rm -rf -- "$RUNTIME_DIR"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export XDG_RUNTIME_DIR="$RUNTIME_DIR"
export WAYLAND_DISPLAY="wayland-virtual-$$"
export DBUS_SESSION_BUS_ADDRESS="unix:path=$RUNTIME_DIR/bus"
export PIPEWIRE_RUNTIME_DIR="$RUNTIME_DIR"
export PIPEWIRE_REMOTE=pipewire-0
export XDG_SESSION_TYPE=wayland
export XDG_CURRENT_DESKTOP=KDE
export XDG_SESSION_DESKTOP=KDE
export KDE_FULL_SESSION=true
export QT_QPA_PLATFORM=wayland
export GDK_BACKEND=wayland
export SDL_VIDEODRIVER=wayland
export MOZ_ENABLE_WAYLAND=1
export GTK_A11Y=1
export NO_AT_BRIDGE=0
export COMPUTER_USE_MCP_ISOLATED=1
export COMPUTER_USE_MCP_VIRTUAL_SESSION=1
export COMPUTER_USE_MCP_DISPLAY="$WAYLAND_DISPLAY"
export COMPUTER_USE_MCP_ISOLATION_MARKER="$ISOLATION_MARKER"

# Do not let clients discover the physical X11 display, accessibility bus, or
# session bus.  The private AT-SPI value is exported after these are removed.
unset DISPLAY XAUTHORITY AT_SPI_BUS_ADDRESS DBUS_STARTER_ADDRESS DBUS_STARTER_BUS_TYPE \
    DBUS_SESSION_BUS_PID DBUS_SESSION_BUS_WINDOWID DBUS_SYSTEM_BUS_ADDRESS
export AT_SPI_BUS_ADDRESS="unix:path=$RUNTIME_DIR/at-spi/bus"

# Prevent KWin, Qt, and portal state from being written to the physical
# session's user directories.  System data remains available through the
# inherited XDG_DATA_DIRS, while this session's writable state is disposable.
export XDG_CONFIG_HOME="$RUNTIME_DIR/config"
export XDG_DATA_HOME="$RUNTIME_DIR/data"
export XDG_STATE_HOME="$RUNTIME_DIR/state"
export KDEHOME="$RUNTIME_DIR/kdehome"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$KDEHOME" "$RUNTIME_DIR/at-spi"

start_owned() {
    local label="$1"
    shift
    local pid
    "$SETSID_BIN" --wait -- "$@" >"$RUNTIME_DIR/$label.log" 2>&1 &
    pid="$!"
    SERVICE_LABELS+=("$label")
    SERVICE_PIDS+=("$pid")
    SERVICE_PID_BY_LABEL["$label"]="$pid"
}

process_alive() {
    local state
    state="$(ps -o stat= -p "$1" 2>/dev/null || true)"
    [ -n "$state" ] && [[ "$state" != Z* ]]
}

wait_for_socket() {
    local label="$1"
    local socket="$2"
    local deadline=$((SECONDS + SOCKET_TIMEOUT_SECS))
    while :; do
        if [ -S "$socket" ]; then
            return 0
        fi
        if ! process_alive "${SERVICE_PID_BY_LABEL[$label]}"; then
            printf 'run-isolated-session.sh: %s exited before creating socket %s\n' "$label" "$socket" >&2
            return 1
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            printf 'run-isolated-session.sh: %s socket was not created: %s\n' "$label" "$socket" >&2
            return 1
        fi
        sleep 0.05
    done
}

wait_for_process() {
    local label="$1"
    while :; do
        if ! process_alive "${SERVICE_PID_BY_LABEL[$label]}"; then
            printf 'run-isolated-session.sh: %s exited during startup\n' "$label" >&2
            return 1
        fi
        # A successful exec that remains alive is sufficient for services with
        # no readiness socket.  Give the process one scheduler turn first.
        sleep 0.05
        if process_alive "${SERVICE_PID_BY_LABEL[$label]}"; then
            return 0
        fi
        printf 'run-isolated-session.sh: %s exited during startup\n' "$label" >&2
        return 1
    done
}

wait_for_bus_name() {
    local label="$1"
    local address="$2"
    local name="$3"
    local reply
    local deadline=$((SECONDS + SOCKET_TIMEOUT_SECS))
    while :; do
        if ! process_alive "${SERVICE_PID_BY_LABEL[$label]}"; then
            printf 'run-isolated-session.sh: %s exited before owning D-Bus name %s\n' "$label" "$name" >&2
            return 1
        fi
        if reply="$("$DBUS_SEND_BIN" --bus="$address" --print-reply=literal \
            --dest=org.freedesktop.DBus /org/freedesktop/DBus \
            org.freedesktop.DBus.NameHasOwner "string:$name" 2>/dev/null)"; then
            case "$reply" in
                *true*)
                    # Do not accept a positive probe from a service that has
                    # already exited but whose setsid wrapper has not been
                    # reaped yet.
                    sleep 0.05
                    if process_alive "${SERVICE_PID_BY_LABEL[$label]}"; then
                        return 0
                    fi
                    printf 'run-isolated-session.sh: %s exited during startup\n' "$label" >&2
                    return 1
                    ;;
            esac
        fi
        if [ "$SECONDS" -ge "$deadline" ]; then
            printf 'run-isolated-session.sh: %s did not own D-Bus name %s\n' "$label" "$name" >&2
            return 1
        fi
        sleep 0.05
    done
}

install_private_portal_authorization() {
    [ "$PORTAL_AUTH_MODE" != off ] || return 0

    if [ -z "$GDBUS_BIN" ]; then
        printf 'run-isolated-session.sh: private KDE RemoteDesktop authorization unavailable; gdbus is not installed, consent may be required\n' >&2
        return 0
    fi

    local permission_store='org.freedesktop.impl.portal.PermissionStore'
    local permission_path='/org/freedesktop/impl/portal/PermissionStore'
    local permission_interface='org.freedesktop.impl.portal.PermissionStore'
    local lookup_output
    local error_output

    if ! error_output="$({
        "$GDBUS_BIN" call \
            --address "$DBUS_SESSION_BUS_ADDRESS" \
            --dest "$permission_store" \
            --object-path "$permission_path" \
            --method "$permission_interface.SetPermission" \
            kde-authorized true remote-desktop '' "['yes']" >/dev/null
        lookup_output="$({
            "$GDBUS_BIN" call \
                --address "$DBUS_SESSION_BUS_ADDRESS" \
                --dest "$permission_store" \
                --object-path "$permission_path" \
                --method "$permission_interface.Lookup" \
                kde-authorized remote-desktop
        } 2>&1)"
        case "$lookup_output" in
            *"['yes']"*)
                printf '%s' "$lookup_output"
                ;;
            *)
                printf 'permission lookup did not contain the private grant: %s' "$lookup_output" >&2
                return 1
                ;;
        esac
    } 2>&1)"; then
        if [ "$PORTAL_AUTH_MODE" = require ]; then
            printf 'run-isolated-session.sh: private KDE RemoteDesktop authorization failed: %s\n' "$error_output" >&2
            return 1
        fi
        printf 'run-isolated-session.sh: private KDE RemoteDesktop authorization unavailable; consent may be required: %s\n' "$error_output" >&2
        return 0
    fi

    printf 'run-isolated-session.sh: private KDE RemoteDesktop authorization installed\n' >&2
}

start_owned dbus "$DBUS_DAEMON_BIN" \
    --session --nofork --nopidfile --address="$DBUS_SESSION_BUS_ADDRESS"
wait_for_socket dbus "$RUNTIME_DIR/bus"
wait_for_bus_name dbus "$DBUS_SESSION_BUS_ADDRESS" org.freedesktop.DBus
install_private_portal_authorization

# KWin makes its own /proc environment unreadable after startup.  Keep a
# launcher-owned supervisor in the process group so the marker can still prove
# the private launch environment without weakening checks for other services.
start_owned compositor "$BASH" -c 'set +e; "$@" & child="$!"; trap "kill -TERM $child 2>/dev/null || true; wait $child 2>/dev/null || true; exit 143" TERM INT; wait "$child"; status="$?"; trap - TERM INT; exit "$status"' isolated-compositor "$COMPOSITOR_BIN" \
    --virtual --socket "$WAYLAND_DISPLAY" \
    --width "$WIDTH" --height "$HEIGHT" --scale "$SCALE" \
    --output-count 1 --no-lockscreen --no-global-shortcuts --no-kactivities
wait_for_socket compositor "$RUNTIME_DIR/$WAYLAND_DISPLAY"
wait_for_bus_name compositor "$DBUS_SESSION_BUS_ADDRESS" org.kde.KWin

start_owned pipewire "$PIPEWIRE_BIN"
wait_for_socket pipewire "$RUNTIME_DIR/pipewire-0"
start_owned wireplumber "$WIREPLUMBER_BIN"
wait_for_process wireplumber

# AT-SPI has a second, accessibility-only bus.  The launcher creates it below
# XDG_RUNTIME_DIR/at-spi; the explicit address keeps clients out of the
# physical accessibility bus and lets the registry be owned by this process
# group rather than by ambient D-Bus activation.
start_owned atspi_bus "$ATSPI_BUS_BIN" --launch-immediately --a11y=1
wait_for_socket atspi_bus "$RUNTIME_DIR/at-spi/bus"
start_owned atspi_registry "$ATSPI_REGISTRY_BIN" --dbus-name org.a11y.atspi.Registry
wait_for_bus_name atspi_registry "$AT_SPI_BUS_ADDRESS" org.a11y.atspi.Registry
wait_for_bus_name atspi_bus "$DBUS_SESSION_BUS_ADDRESS" org.a11y.Bus

# Start both sides explicitly.  The private bus is not launched with
# systemd activation, so the SystemdService field in portal service files
# cannot route activation to the ambient per-user service manager.
start_owned portal_backend "$PORTAL_BACKEND_BIN"
wait_for_bus_name portal_backend "$DBUS_SESSION_BUS_ADDRESS" org.freedesktop.impl.portal.desktop.kde
start_owned portal "$PORTAL_BIN" --replace
wait_for_bus_name portal "$DBUS_SESSION_BUS_ADDRESS" org.freedesktop.portal.Desktop

marker_tmp="$RUNTIME_DIR/isolation.ready.$$"
{
    printf 'version=1\n'
    printf 'runtime_dir=%s\n' "$RUNTIME_DIR"
    printf 'wayland_display=%s\n' "$WAYLAND_DISPLAY"
    printf 'dbus_address=%s\n' "$DBUS_SESSION_BUS_ADDRESS"
    printf 'at_spi_bus_address=%s\n' "$AT_SPI_BUS_ADDRESS"
    printf 'pipewire_remote=%s\n' "$PIPEWIRE_REMOTE"
    printf 'pipewire_socket=%s\n' "$RUNTIME_DIR/pipewire-0"
    printf 'launcher_pid=%s\n' "$$"
    printf 'dbus_pid=%s\n' "${SERVICE_PID_BY_LABEL[dbus]}"
    printf 'compositor_pid=%s\n' "${SERVICE_PID_BY_LABEL[compositor]}"
    printf 'pipewire_pid=%s\n' "${SERVICE_PID_BY_LABEL[pipewire]}"
    printf 'wireplumber_pid=%s\n' "${SERVICE_PID_BY_LABEL[wireplumber]}"
    printf 'atspi_bus_pid=%s\n' "${SERVICE_PID_BY_LABEL[atspi_bus]}"
    printf 'atspi_registry_pid=%s\n' "${SERVICE_PID_BY_LABEL[atspi_registry]}"
    printf 'portal_backend_pid=%s\n' "${SERVICE_PID_BY_LABEL[portal_backend]}"
    printf 'portal_pid=%s\n' "${SERVICE_PID_BY_LABEL[portal]}"
} >"$marker_tmp"
chmod 600 "$marker_tmp"
mv -- "$marker_tmp" "$ISOLATION_MARKER"

printf 'run-isolated-session.sh: isolated session ready (display=%s %sx%s@%sx)\n' \
    "$WAYLAND_DISPLAY" "$WIDTH" "$HEIGHT" "$SCALE" >&2

# Keep the shell alive so EXIT cleanup owns the full service lifetime.  The
# executable at COMMAND[0] is the actual COMPUTER_USE_MCP_BIN override when
# one was supplied.
set +e
"${COMMAND[@]}"
status="$?"
set -e
exit "$status"
