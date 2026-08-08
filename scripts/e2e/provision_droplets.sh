#!/usr/bin/env bash
# Provision (and destroy) the two-droplet lab that scenario N needs.
#
# Scenario L proves a Mac and one droplet can share a session. It cannot prove two
# *other* machines can, because this Mac is always one of the two peers, and the peer
# that hosts the coordinator is always the one whose ticket we can read locally. This
# script stands up two throwaway droplets in different regions so the whole session --
# coordinator, guest, both PTYs -- lives on the public internet, with this Mac only
# holding the ssh reins.
#
# Every resource it creates carries the p2pmux-itest tag, and `destroy` removes exactly
# those. Pre-existing droplets and keys are never touched: the tag is the contract.
#
#   ./provision_droplets.sh create     # ~8 minutes, mostly the release build
#   ./provision_droplets.sh status
#   ./provision_droplets.sh destroy    # run this when you are done, always
#
# Requires: doctl authenticated against an account with droplet create rights.
set -euo pipefail

TAG="p2pmux-itest"
KEY_NAME="p2pmux-itest"
SIZE="${P2PMUX_ITEST_SIZE:-s-4vcpu-8gb}"
IMAGE="ubuntu-24-04-x64"
# Two continents, so a passing run says something about real paths rather than about
# one datacenter's internal network.
REGION_A="${P2PMUX_ITEST_REGION_A:-nyc3}"
REGION_B="${P2PMUX_ITEST_REGION_B:-fra1}"
NAME_A="p2pmux-itest-a"
NAME_B="p2pmux-itest-b"

STATE_DIR="${P2PMUX_ITEST_STATE:-$HOME/.cache/p2pmux/itest}"
KEY_FILE="$STATE_DIR/id_$KEY_NAME"
MANIFEST="$STATE_DIR/droplets.json"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REMOTE_BINARY="/usr/local/bin/p2pmux"

ssh_to() {
  local host="$1"; shift
  ssh -i "$KEY_FILE" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=15 \
      "root@$host" "$@"
}

scp_to() {
  scp -i "$KEY_FILE" -o IdentitiesOnly=yes -o StrictHostKeyChecking=no \
      -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR "$@"
}

droplet_ip() {
  doctl compute droplet list --tag-name "$TAG" --format Name,PublicIPv4 --no-header |
    awk -v name="$1" '$1 == name { print $2 }'
}

# `create --wait` returns before the address is readable back out of the list API,
# so a droplet can be active and still list a blank IP for a few seconds. Reading
# it once handed `wait_for_bootstrap` an empty host, which then spent ten minutes
# failing to ssh to `root@` before the run died — with the droplets already paid
# for and no binary on either.
require_droplet_ip() {
  local name="$1" ip
  for _ in $(seq 1 20); do
    ip="$(droplet_ip "$name")"
    if [[ -n "$ip" ]]; then
      echo "$ip"
      return 0
    fi
    sleep 5
  done
  echo "no address for droplet $name after 100s" >&2
  return 1
}

cloud_init() {
  cat <<'YAML'
#cloud-config
package_update: true
packages:
  - build-essential
  - pkg-config
  - libssl-dev
  - git
  - curl
runcmd:
  - [ sh, -c, "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal" ]
  - [ sh, -c, "touch /root/.p2pmux-bootstrap-done" ]
YAML
}

wait_for_bootstrap() {
  local ip="$1" tries=40
  if [[ -z "$ip" ]]; then
    echo "wait_for_bootstrap called with no address" >&2
    return 1
  fi
  for _ in $(seq 1 $tries); do
    if ssh_to "$ip" 'test -f /root/.p2pmux-bootstrap-done' 2>/dev/null; then
      return 0
    fi
    sleep 15
  done
  echo "droplet $ip never finished cloud-init" >&2
  return 1
}

create() {
  mkdir -p "$STATE_DIR"
  chmod 700 "$STATE_DIR"

  if [[ ! -f "$KEY_FILE" ]]; then
    ssh-keygen -t ed25519 -N '' -f "$KEY_FILE" -C "$KEY_NAME" >/dev/null
  fi
  local key_id
  key_id="$(doctl compute ssh-key list --format ID,Name --no-header |
    awk -v name="$KEY_NAME" '$2 == name { print $1 }' | head -1)"
  if [[ -z "$key_id" ]]; then
    key_id="$(doctl compute ssh-key import "$KEY_NAME" --public-key-file "$KEY_FILE.pub" \
      --format ID --no-header)"
  fi

  local init_file="$STATE_DIR/cloud-init.yaml"
  cloud_init >"$init_file"

  for spec in "$NAME_A:$REGION_A" "$NAME_B:$REGION_B"; do
    local name="${spec%%:*}" region="${spec##*:}"
    if [[ -n "$(droplet_ip "$name")" ]]; then
      echo "$name already exists, reusing"
      continue
    fi
    echo "creating $name in $region"
    doctl compute droplet create "$name" --region "$region" --size "$SIZE" \
      --image "$IMAGE" --ssh-keys "$key_id" --tag-name "$TAG" \
      --user-data-file "$init_file" --wait --format Name,PublicIPv4 --no-header
  done

  local ip_a ip_b
  ip_a="$(require_droplet_ip "$NAME_A")"
  ip_b="$(require_droplet_ip "$NAME_B")"
  echo "waiting for cloud-init on $ip_a and $ip_b"
  wait_for_bootstrap "$ip_a"
  wait_for_bootstrap "$ip_b"

  # Build once, copy twice: both droplets are the same image and architecture, and the
  # release build is by far the slowest step here.
  if ! ssh_to "$ip_a" "test -x $REMOTE_BINARY"; then
    echo "shipping source to $NAME_A"
    tar --exclude target --exclude .git -czf "$STATE_DIR/src.tgz" -C "$REPO_ROOT" .
    scp_to "$STATE_DIR/src.tgz" "root@$ip_a:/root/src.tgz"
    ssh_to "$ip_a" 'mkdir -p /root/p2pmux && tar -xzf /root/src.tgz -C /root/p2pmux'
    echo "building (this is the slow part)"
    ssh_to "$ip_a" 'cd /root/p2pmux && . /root/.cargo/env && cargo build --release 2>&1 | tail -3'
    ssh_to "$ip_a" "cp /root/p2pmux/target/release/p2pmux $REMOTE_BINARY"
  fi
  if ! ssh_to "$ip_b" "test -x $REMOTE_BINARY"; then
    scp_to "root@$ip_a:$REMOTE_BINARY" "$STATE_DIR/p2pmux-linux"
    scp_to "$STATE_DIR/p2pmux-linux" "root@$ip_b:$REMOTE_BINARY"
    ssh_to "$ip_b" "chmod +x $REMOTE_BINARY"
  fi

  cat >"$MANIFEST" <<JSON
{
  "key_file": "$KEY_FILE",
  "binary": "$REMOTE_BINARY",
  "hosts": [
    {"alias": "$NAME_A", "hostname": "$ip_a", "region": "$REGION_A"},
    {"alias": "$NAME_B", "hostname": "$ip_b", "region": "$REGION_B"}
  ]
}
JSON
  echo "manifest: $MANIFEST"
  cat "$MANIFEST"
}

status() {
  doctl compute droplet list --tag-name "$TAG" --format ID,Name,Region,Status,PublicIPv4
  [[ -f "$MANIFEST" ]] && cat "$MANIFEST"
}

destroy() {
  local ids
  ids="$(doctl compute droplet list --tag-name "$TAG" --format ID --no-header || true)"
  for id in $ids; do
    echo "destroying droplet $id"
    doctl compute droplet delete "$id" --force
  done
  local key_id
  key_id="$(doctl compute ssh-key list --format ID,Name --no-header |
    awk -v name="$KEY_NAME" '$2 == name { print $1 }' | head -1)"
  if [[ -n "$key_id" ]]; then
    echo "destroying ssh key $key_id"
    doctl compute ssh-key delete "$key_id" --force
  fi
  rm -f "$MANIFEST"
  echo "remaining droplets tagged $TAG:"
  doctl compute droplet list --tag-name "$TAG" --format ID,Name --no-header || true
}

case "${1:-}" in
  create) create ;;
  status) status ;;
  destroy) destroy ;;
  *)
    echo "usage: $0 {create|status|destroy}" >&2
    exit 2
    ;;
esac
