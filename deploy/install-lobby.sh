#!/bin/sh
# Installs sadk-lobby as a systemd service (Debian/Ubuntu, Fedora/RHEL and
# other systemd distributions). Run as root.
#
#   sudo sh deploy/install-lobby.sh --msgdefs /path/to/msgdefs.ini --public-host lobby.example.org
#
# --msgdefs PATH    msgdefs.ini from a retail install's bin folder (required
#                   on the first run; kept afterwards)
# --public-host H   the DNS name or IPv4 address players connect to
# --binary PATH     the sadk-lobby binary (default: target/release/sadk-lobby,
#                   or sadk-lobby-<arch> next to this script)
# --port N          lobby base port; the lobby uses N..N+3 (default 17070)
# --match-forward   also run sadk-match-forward: public TCP 5479 ->
#                   127.0.0.1:15479, for a match host that tunnels its game
#                   port to the server over SSH (docs/PLAYING-ONLINE.md)
#
# Running it again updates the binary and the unit and restarts the service;
# /var/lib/sadk/lobby-data (accounts) is kept.
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
PUBLIC_HOST=""
PORT=17070
MATCH_FORWARD=0
MSGDEFS=""
BIN=""
while [ $# -gt 0 ]; do
    case "$1" in
        --public-host) PUBLIC_HOST="$2"; shift 2 ;;
        --port) PORT="$2"; shift 2 ;;
        --msgdefs) MSGDEFS="$2"; shift 2 ;;
        --binary) BIN="$2"; shift 2 ;;
        --match-forward) MATCH_FORWARD=1; shift ;;
        -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
[ -n "$PUBLIC_HOST" ] || { echo "--public-host is required" >&2; exit 2; }
[ "$(id -u)" = 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
LAST=$((PORT + 3))

if [ -z "$BIN" ]; then
    if [ -f "$HERE/../target/release/sadk-lobby" ]; then
        BIN="$HERE/../target/release/sadk-lobby"
    else
        case "$(uname -m)" in
            x86_64|amd64) BIN="$HERE/sadk-lobby-x86_64" ;;
            aarch64|arm64) BIN="$HERE/sadk-lobby-aarch64" ;;
            *) BIN="$HERE/sadk-lobby-$(uname -m)" ;;
        esac
    fi
fi
[ -f "$BIN" ] || { echo "no sadk-lobby binary at $BIN (cargo build --release, or pass --binary)" >&2; exit 1; }
if [ -z "$MSGDEFS" ] && [ ! -f /opt/sadk/msgdefs.ini ]; then
    echo "--msgdefs is required (bin/msgdefs.ini from a retail install)" >&2; exit 2
fi

echo "== user and folders"
id sadk >/dev/null 2>&1 || useradd --system --home /var/lib/sadk --shell /usr/sbin/nologin sadk
install -d -m 755 /opt/sadk
install -d -o sadk -g sadk -m 750 /var/lib/sadk /var/lib/sadk/lobby-data /var/lib/sadk/backup
install -m 755 "$BIN" /opt/sadk/sadk-lobby
[ -z "$MSGDEFS" ] || install -m 644 "$MSGDEFS" /opt/sadk/msgdefs.ini
if command -v getenforce >/dev/null 2>&1 && [ "$(getenforce)" = Enforcing ]; then
    chcon -t bin_t /opt/sadk/sadk-lobby || true
fi

echo "== systemd unit"
cat > /etc/systemd/system/sadk-lobby.service <<EOF
[Unit]
Description=Die Siedler - Aufbruch der Kulturen lobby (sadk-lobby)
After=network-online.target
Wants=network-online.target

[Service]
User=sadk
Group=sadk
WorkingDirectory=/var/lib/sadk
Environment=RUST_LOG=sadk_lobby=info
ExecStart=/opt/sadk/sadk-lobby --msgdefs /opt/sadk/msgdefs.ini --listen 0.0.0.0:$PORT --data /var/lib/sadk/lobby-data --public-host $PUBLIC_HOST
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/sadk
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF

if [ "$MATCH_FORWARD" = 1 ]; then
    echo "== match forwarder (5479 -> 127.0.0.1:15479)"
    if ! command -v socat >/dev/null 2>&1; then
        if command -v apt-get >/dev/null 2>&1; then DEBIAN_FRONTEND=noninteractive apt-get install -y socat
        elif command -v dnf >/dev/null 2>&1; then dnf install -y socat
        else echo "install socat by hand" >&2; exit 1; fi
    fi
    cat > /etc/systemd/system/sadk-match-forward.service <<EOF
[Unit]
Description=Forward public TCP 5479 to a match host's SSH reverse tunnel on 127.0.0.1:15479
After=network-online.target
Wants=network-online.target

[Service]
User=sadk
Group=sadk
ExecStart=$(command -v socat) TCP4-LISTEN:5479,fork,reuseaddr TCP4:127.0.0.1:15479
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
fi

echo "== backups of accounts.json (every 30 min, kept 14 days)"
cat > /etc/cron.d/sadk-backup <<'EOF'
*/30 * * * * sadk  [ -f /var/lib/sadk/lobby-data/accounts.json ] && cp /var/lib/sadk/lobby-data/accounts.json /var/lib/sadk/backup/accounts-$(date +\%Y\%m\%d-\%H\%M).json && find /var/lib/sadk/backup -name 'accounts-*.json' -mtime +14 -delete
EOF

echo "== firewall"
if command -v ufw >/dev/null 2>&1 && ufw status | grep -q "Status: active"; then
    ufw allow "$PORT:$LAST/tcp"
    if [ "$MATCH_FORWARD" = 1 ]; then ufw allow 5479/tcp; fi
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --permanent --add-port="$PORT-$LAST/tcp"
    if [ "$MATCH_FORWARD" = 1 ]; then firewall-cmd --permanent --add-port=5479/tcp; fi
    firewall-cmd --reload
else
    echo "no active ufw/firewalld; if iptables/nftables filter input, open TCP $PORT-$LAST (and 5479) yourself"
fi
echo "   A firewall at your hosting provider must allow these ports too."

echo "== start"
systemctl daemon-reload
systemctl enable sadk-lobby >/dev/null 2>&1
systemctl restart sadk-lobby
if [ "$MATCH_FORWARD" = 1 ]; then
    systemctl enable sadk-match-forward >/dev/null 2>&1
    systemctl restart sadk-match-forward
fi
sleep 2
systemctl --no-pager --lines=0 status sadk-lobby | head -5
ss -ltn | grep -E ":($PORT|$((PORT+1))|$((PORT+2))|$LAST|5479) " || true
journalctl -u sadk-lobby -n 5 --no-pager -o cat
echo "done: players point their game at $PUBLIC_HOST port $PORT"
