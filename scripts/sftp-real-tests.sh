#!/usr/bin/env bash
# Run the SFTP tests that need a real sshd (sftp::real_server, ignored by
# default) against a throwaway OpenSSH server: temporary host and user keys,
# a high port on 127.0.0.1, running as you. Works on Linux, macOS and in Git
# Bash on Windows (Git for Windows ships sshd). Nothing touches ~/.ssh.
#
#   scripts/sftp-real-tests.sh            # the integration tests
#   scripts/sftp-real-tests.sh --bench    # plus the throughput benchmark (release)
set -euo pipefail

port="${COREPTY_SSHD_PORT:-2290}"
work="$(mktemp -d)"
trap 'kill "$(cat "$work/sshd.pid" 2>/dev/null)" 2>/dev/null || true; rm -rf "$work"' EXIT

sshd_bin="$(command -v sshd || true)"
for candidate in /usr/sbin/sshd /usr/bin/sshd; do
  [ -z "$sshd_bin" ] && [ -x "$candidate" ] && sshd_bin="$candidate"
done
[ -n "$sshd_bin" ] || { echo "no sshd found" >&2; exit 1; }
sftp_server=""
for candidate in /usr/lib/openssh/sftp-server /usr/libexec/openssh/sftp-server \
  /usr/libexec/sftp-server /usr/lib/ssh/sftp-server; do
  [ -x "$candidate" ] && sftp_server="$candidate" && break
done
[ -n "$sftp_server" ] || { echo "no sftp-server found" >&2; exit 1; }

ssh-keygen -q -t ed25519 -N "" -f "$work/host_key"
ssh-keygen -q -t ed25519 -N "" -f "$work/user_key"
cp "$work/user_key.pub" "$work/authorized_keys"
mkdir "$work/root"
cat > "$work/sshd_config" <<EOF
Port $port
ListenAddress 127.0.0.1
HostKey $work/host_key
AuthorizedKeysFile $work/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
StrictModes no
PidFile $work/sshd.pid
Subsystem sftp $sftp_server
EOF
"$sshd_bin" -f "$work/sshd_config" -E "$work/sshd.log"
for _ in $(seq 50); do [ -s "$work/sshd.pid" ] && break; sleep 0.1; done

key="$work/user_key"
command -v cygpath >/dev/null && key="$(cygpath -w "$key")"
export COREPTY_SSHD="127.0.0.1:$port" COREPTY_SSHD_USER="$(id -un)" \
  COREPTY_SSHD_KEY="$key" COREPTY_SSHD_DIR="$work/root"

cd "$(dirname "$0")/../src-tauri"
cargo test --lib sftp::real_server -- --ignored --nocapture
if [ "${1:-}" = "--bench" ]; then
  COREPTY_BENCH_MB="${COREPTY_BENCH_MB:-256}" \
    cargo test --release --lib sftp::bench -- --ignored --nocapture
fi
