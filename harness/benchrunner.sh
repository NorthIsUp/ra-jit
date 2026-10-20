#!/usr/bin/env bash
# benchrunner.sh: makes this Linux x86-64 machine the self-hosted runner of indexable-inc/ra-jit
# that the bench-readme workflow runs on (labels self-hosted, linux, x64, bench-x86).
#
#   gh api -X POST repos/indexable-inc/ra-jit/actions/runners/registration-token --jq .token \
#     | ssh HOST bash -s -- "'AWS c8a.4xlarge'" < /dev/null   # see docs/benchmarks.md for the exact form
#
# Reads the registration token (valid one hour, needs repo admin) from stdin; the argument names
# the machine for the results (RA_JIT_BENCH_MACHINE). Installs the runner in ~/actions-runner and
# runs it from a user systemd unit, ra-jit-bench-runner.service, that lingers (starts at boot).
# Needs curl, tar and Nix with flakes; on NixOS the runner's own binaries run through nix-ld.
# Run again to re-register (--replace keeps the name).
set -euo pipefail
export PATH=/run/current-system/sw/bin:/run/wrappers/bin:$PATH
machine=${1:?name the machine, e.g. "AWS c8a.4xlarge"}
read -r token
[ -n "$token" ] || { echo "no registration token on stdin" >&2; exit 1; }
dir=$HOME/actions-runner
version=$(curl -fsSL https://api.github.com/repos/actions/runner/releases/latest | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p')
mkdir -p "$dir"
cd "$dir"
if [ ! -x ./run.sh ] || [ "$(cat .version 2>/dev/null)" != "$version" ]; then
  curl -fsSL "https://github.com/actions/runner/releases/download/v$version/actions-runner-linux-x64-$version.tar.gz" | tar xz
  echo "$version" > .version
fi
# The runner is .NET: without ICU on the host it has to be told to do without.
export DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
./config.sh --unattended --replace --url https://github.com/indexable-inc/ra-jit --token "$token" \
  --name "$(hostname)" --labels bench-x86 --work _work
cat > .env <<EOF
RA_JIT_BENCH_MACHINE=$machine
DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
EOF
mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/ra-jit-bench-runner.service" <<EOF
[Unit]
Description=GitHub Actions runner for ra-jit benchmarks (bench-x86)
After=network-online.target

[Service]
WorkingDirectory=$dir
ExecStart=$dir/run.sh
Environment=PATH=$HOME/.nix-profile/bin:/nix/var/nix/profiles/default/bin:/run/current-system/sw/bin:/run/wrappers/bin
Environment=DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now ra-jit-bench-runner.service
loginctl enable-linger "$USER" 2>/dev/null || sudo loginctl enable-linger "$USER"
systemctl --user --no-pager status ra-jit-bench-runner.service | head -5
