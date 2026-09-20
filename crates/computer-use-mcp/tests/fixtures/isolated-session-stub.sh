#!/usr/bin/env bash
set -u

role="$(basename "$0")"

if [ "$role" = "dbus-send" ]; then
    printf '   boolean true\n'
    exit 0
fi

if [ "${STUB_FAIL_ROLE-}" = "$role" ]; then
    exit 91
fi

if [ -n "${STUB_PID_LOG-}" ]; then
    printf '%s %s %s\n' "$role" "$$" "$PPID" >>"$STUB_PID_LOG"
fi

if [ -n "${STUB_ENV_DIR-}" ]; then
    {
        printf 'role=%s\n' "$role"
        for variable in \
            AT_SPI_BUS_ADDRESS \
            COMPUTER_USE_MCP_DISPLAY \
            COMPUTER_USE_MCP_ISOLATED \
             COMPUTER_USE_MCP_ISOLATION_MARKER \
             COMPUTER_USE_MCP_VIRTUAL_SESSION \
             DBUS_STARTER_ADDRESS \
             DBUS_STARTER_BUS_TYPE \
             DBUS_SESSION_BUS_ADDRESS \
            DBUS_SESSION_BUS_PID \
            DBUS_SESSION_BUS_WINDOWID \
            DBUS_SYSTEM_BUS_ADDRESS \
            DISPLAY \
            GDK_BACKEND \
            KDE_FULL_SESSION \
            NO_AT_BRIDGE \
            PIPEWIRE_REMOTE \
            PIPEWIRE_RUNTIME_DIR \
            QT_QPA_PLATFORM \
            WAYLAND_DISPLAY \
            XAUTHORITY \
            XDG_CONFIG_HOME \
            XDG_CURRENT_DESKTOP \
            XDG_DATA_HOME \
            XDG_RUNTIME_DIR \
            XDG_SESSION_TYPE \
            XDG_STATE_HOME; do
            if [[ -v "$variable" ]]; then
                printf '%s=%s\n' "$variable" "${!variable}"
            else
                printf '%s=<unset>\n' "$variable"
            fi
        done
    } >"$STUB_ENV_DIR/$role"
fi

case "$role" in
    dbus-daemon)
        socket_path="$XDG_RUNTIME_DIR/bus"
        ;;
    kwin_wayland)
        socket_path="$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
        ;;
    pipewire)
        socket_path="$XDG_RUNTIME_DIR/pipewire-0"
        ;;
    at-spi-bus-launcher)
        socket_path="$XDG_RUNTIME_DIR/at-spi/bus"
        ;;
    server)
        printf 'server_arg_count=%s\n' "$#" >>"$STUB_ENV_DIR/server"
        for argument in "$@"; do
            printf 'server_arg=%s\n' "$argument" >>"$STUB_ENV_DIR/server"
        done
        sleep 30 &
        printf 'server-orphan %s %s\n' "$!" "$PPID" >>"$STUB_PID_LOG"
        exit 0
        ;;
    *)
        socket_path=""
        ;;
esac

if [ -n "$socket_path" ]; then
    python3 - "$socket_path" <<'PY' &
import socket
import sys

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(sys.argv[1])
server.listen(1)
while True:
    connection, _ = server.accept()
    connection.close()
PY
    socket_pid="$!"
else
    socket_pid=""
fi

cleanup() {
    if [ -n "$socket_pid" ]; then
        kill "$socket_pid" 2>/dev/null || true
        wait "$socket_pid" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

while :; do
    sleep 1
done
