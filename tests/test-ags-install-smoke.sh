#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_target="${CARGO_TARGET_DIR:-$project_root/target}"
binary="${CASR_TEST_BINARY:-$cargo_target/debug/casr}"
platform="$(uname -s)"
test_tmp_root=/tmp
[[ "$platform" != Darwin ]] || test_tmp_root=/private/tmp
tmp="$(mktemp -d "$test_tmp_root/ags-install-smoke.XXXXXX")"
export FAKE_REAL_NODE_BINARY="$(command -v node)"
[[ -x "$FAKE_REAL_NODE_BINARY" ]]
export FAKE_CONTEXT_RUNTIME_PLATFORM="$(
    "$FAKE_REAL_NODE_BINARY" -p 'process.platform'
)"
export FAKE_CONTEXT_RUNTIME_ARCH="$(
    "$FAKE_REAL_NODE_BINARY" -p 'process.arch'
)"
export FAKE_CONTEXT_RUNTIME_NODE_ABI="$(
    "$FAKE_REAL_NODE_BINARY" -p 'process.versions.modules'
)"
export FAKE_CONTEXT_RUNTIME_TARGET="$(
    printf '%s-%s-node%s' \
        "$FAKE_CONTEXT_RUNTIME_PLATFORM" \
        "$FAKE_CONTEXT_RUNTIME_ARCH" \
        "$FAKE_CONTEXT_RUNTIME_NODE_ABI"
)"

cleanup() {
    case "$tmp" in
        /tmp/ags-install-smoke.*|/private/tmp/ags-install-smoke.*) rm -rf -- "$tmp" ;;
    esac
}
trap cleanup EXIT

[[ -x "$binary" ]] || {
    printf 'CASR test binary is missing: %s\n' "$binary" >&2
    exit 1
}

test_sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$1" | awk '{print $1}'
    else
        shasum -a 256 -- "$1" | awk '{print $1}'
    fi
}

write_rmux_recovery_journal() {
    local journal="$1" dest="$2" prefix="$3"
    local candidate_client="$4" candidate_daemon="$5" candidate_helper="$6"
    local previous_client="$7" previous_daemon="$8" previous_helper="$9"
    local suffix="${10}"
    jq -n \
        --arg version 0.9.1 \
        --arg client "$dest/rmux" \
        --arg daemon "$dest/rmux-daemon" \
        --arg helper "$prefix/libexec/rmux/rmux" \
        --arg suffix "$suffix" \
        --arg candidate_client "$candidate_client" \
        --arg candidate_daemon "$candidate_daemon" \
        --arg candidate_helper "$candidate_helper" \
        --arg previous_client "$previous_client" \
        --arg previous_daemon "$previous_daemon" \
        --arg previous_helper "$previous_helper" \
        --arg backup0 "$dest/.rmux.rollback.0.$suffix" \
        --arg backup1 "$dest/.rmux.rollback.1.$suffix" \
        --arg backup2 "$dest/.rmux.rollback.2.$suffix" '{
          schema:1,
          managed_by:"ags-rmux-installer",
          version:$version,
          entries:[
            {
              path:$client,
              stage_path:($client + ".install-transaction." + $suffix),
              candidate_sha256:$candidate_client,
              previous:{
                existed:true,sha256:$previous_client,backup_path:$backup0
              }
            },
            {
              path:$daemon,
              stage_path:($daemon + ".install-transaction." + $suffix),
              candidate_sha256:$candidate_daemon,
              previous:{
                existed:true,sha256:$previous_daemon,backup_path:$backup1
              }
            },
            {
              path:$helper,
              stage_path:($helper + ".install-transaction." + $suffix),
              candidate_sha256:$candidate_helper,
              previous:{
                existed:true,sha256:$previous_helper,backup_path:$backup2
              }
            }
          ]
        }' > "$journal"
    chmod 600 "$journal"
}

artifact_root="$tmp/artifact"
artifact="$tmp/casr.tar.xz"
home="$tmp/home"
bin_dir="$tmp/bin"
offline_guard_bin="$tmp/offline-guard-bin"
offline_network_marker="$tmp/offline-network-used"
mkdir -p "$artifact_root" "$home/.codex" "$home/.claude" "$offline_guard_bin"
ln -s "$FAKE_REAL_NODE_BINARY" "$offline_guard_bin/node-real"
install -m 0755 "$binary" "$artifact_root/casr"
mkdir -p "$artifact_root/bin" "$artifact_root/libexec/rmux"
cat > "$artifact_root/bin/rmux" <<'EOF'
#!/bin/sh
case "${1:-}" in
    -V) printf 'rmux 0.9.1\n' ;;
    list-commands) exit 0 ;;
    *) exit 64 ;;
esac
EOF
printf '%s\n' '#!/bin/sh' 'exit 0' > "$artifact_root/bin/rmux-daemon"
printf '%s\n' '#!/bin/sh' 'exit 0' > "$artifact_root/libexec/rmux/rmux"
chmod 0755 "$artifact_root/bin/rmux" "$artifact_root/bin/rmux-daemon" \
    "$artifact_root/libexec/rmux/rmux"
tar -cJf "$artifact" -C "$artifact_root" \
    casr bin/rmux bin/rmux-daemon libexec/rmux/rmux

cat > "$offline_guard_bin/curl" <<'EOF'
#!/bin/sh
: > "${OFFLINE_NETWORK_MARKER:?}"
exit 99
EOF
chmod +x "$offline_guard_bin/curl"
cat > "$offline_guard_bin/node" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == --version ]]; then
    printf '%s\n' "${FAKE_NODE_VERSION:-v22.5.0}"
    exit
fi
script="${1:-}"
shift || true
if [[ "$script" == - ]]; then
    real_node="${FAKE_REAL_NODE_BINARY:-${BASH_SOURCE[0]%/*}/node-real}"
    exec "$real_node" - "$@"
fi
if [[ "$script" == */context-mode/hooks/ensure-deps.mjs ]]; then
    real_node="${FAKE_REAL_NODE_BINARY:-${BASH_SOURCE[0]%/*}/node-real}"
    exec "$real_node" "$script" "$@"
fi
[[ "$script" == */context-mode/cli.bundle.mjs ]] || exit 64
printf 'CONTEXT_MODE=%s:%s\n' "${CONTEXT_MODE_PLATFORM:-unknown}" "${1:-}" \
    >> "$HOME/.context-mode-test.log"
case "${1:-}" in
    index)
        if [[ "${FAKE_CONTEXT_HEALTH_FAIL:-}" == "${CONTEXT_MODE_PLATFORM:-}" ]]; then
            exit 65
        fi
        source_file="${2:?}"
        shift 2
        source_label=
        project=
        while (( $# > 0 )); do
            case "$1" in
                --source) source_label="${2:?}"; shift 2 ;;
                --project) project="${2:?}"; shift 2 ;;
                *) exit 64 ;;
            esac
        done
        index_state="${CONTEXT_MODE_DIR:?}/fake-index.json"
        title="$(sed -n 's/^# //p' "$source_file" | head -n 1)"
        content="$(sed -n '3p' "$source_file")"
        jq -n --arg title "$title" --arg content "$content" \
            --arg source "$source_label" --arg project "$project" \
            '{title:$title,content:$content,source:$source,project:$project}' \
            > "$index_state"
        printf 'Indexed 1 sections from %s\nSource: %s\nProject: %s\n' \
            "$source_file" "$source_label" "$project"
        ;;
    search)
        query="${2:?}"
        shift 2
        source_label=
        project=
        while (( $# > 0 )); do
            case "$1" in
                --source) source_label="${2:?}"; shift 2 ;;
                --project) project="${2:?}"; shift 2 ;;
                --limit) [[ "${2:?}" == 1 ]]; shift 2 ;;
                *) exit 64 ;;
            esac
        done
        index_state="${CONTEXT_MODE_DIR:?}/fake-index.json"
        jq -e --arg query "$query" --arg source "$source_label" \
            --arg project "$project" '
              .source == $source and .project == $project and
              (.content | contains($query))
            ' "$index_state" >/dev/null
        jq -r '"## 1. " + .title, "Source: " + .source, .content' \
            "$index_state"
        ;;
    *) exit 64 ;;
esac
EOF
chmod +x "$offline_guard_bin/node"
cat > "$offline_guard_bin/claude" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state="$HOME/.local/state/context-mode-fake"
root="$HOME/.local/share/ags/context-mode/runtimes/${FAKE_CONTEXT_RUNTIME_TARGET:?}/1.0.169/node_modules/context-mode"
if [[ "${1:-}" == --version ]]; then printf 'claude-test 1.0\n'; exit; fi
if [[ " $* " == *" --include-hook-events "* ]]; then
    printf '%s\n' \
        '{"type":"system","subtype":"hook_response","hook_event":"SessionStart","outcome":"success","exit_code":0,"output":"<context_window_protection>ready</context_window_protection>"}' \
        '{"type":"system","subtype":"init","mcp_servers":[{"name":"plugin:context-mode:context-mode","status":"connected"}],"tools":["mcp__plugin_context-mode_context-mode__ctx_execute","mcp__plugin_context-mode_context-mode__ctx_search"],"plugins":[{"source":"context-mode@context-mode","version":"1.0.169"}]}'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == marketplace && "${3:-}" == list ]]; then
    [[ -e "$state/claude-marketplace" ]] &&
        jq -n --arg root "${FAKE_CLAUDE_CONTEXT_MARKETPLACE_ROOT:-$root}" \
            '[{name:"context-mode",source:"directory",path:$root,installLocation:$root}]' ||
        printf '[]\n'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == marketplace && "${3:-}" == add ]]; then
    mkdir -p "$state"; : > "$state/claude-marketplace"
    printf 'CLAUDE_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == marketplace && "${3:-}" == remove ]]; then
    rm -f -- "$state/claude-marketplace"
    printf 'CLAUDE_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == list ]]; then
    cache_root="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins/cache/context-mode/context-mode/1.0.169"
    [[ -e "$state/claude-plugin" ]] &&
        jq -n --arg root "$cache_root" \
            '[{id:"context-mode@context-mode",version:"1.0.169",scope:"user",enabled:true,installPath:$root}]' ||
        printf '[]\n'
    exit
fi
if [[ "${1:-}" == plugin && ( "${2:-}" == install || "${2:-}" == enable ) ]]; then
    mkdir -p "$state"; : > "$state/claude-plugin"
    cache_root="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/plugins/cache/context-mode/context-mode/1.0.169"
    mkdir -p "$cache_root"
    cp -a -- "$root/." "$cache_root/"
    printf 'CLAUDE_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == uninstall ]]; then
    rm -f -- "$state/claude-plugin"
    printf 'CLAUDE_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == disable ]]; then
    rm -f -- "$state/claude-plugin"
    printf 'CLAUDE_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
exit 64
EOF
chmod +x "$offline_guard_bin/claude"
cat > "$offline_guard_bin/codex" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
state="$HOME/.local/state/context-mode-fake"
root="$HOME/.local/share/ags/context-mode/runtimes/${FAKE_CONTEXT_RUNTIME_TARGET:?}/1.0.169/node_modules/context-mode"
if [[ "${1:-}" == --version ]]; then printf 'codex-test 1.0\n'; exit; fi
if [[ "${1:-}" == plugin && "${2:-}" == marketplace && "${3:-}" == list ]]; then
    [[ -e "$state/codex-marketplace" ]] &&
        jq -n --arg root "${FAKE_CODEX_CONTEXT_MARKETPLACE_ROOT:-$root}" '{
          marketplaces:[{name:"context-mode",root:$root,
            marketplaceSource:{sourceType:"local",source:$root}}]
        }' ||
        printf '{"marketplaces":[]}\n'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == marketplace && "${3:-}" == add ]]; then
    mkdir -p "$state"; : > "$state/codex-marketplace"
    printf 'CODEX_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    printf '{"ok":true}\n'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == marketplace && "${3:-}" == remove ]]; then
    rm -f -- "$state/codex-marketplace"
    printf 'CODEX_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    printf '{"ok":true}\n'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == list ]]; then
    [[ -e "$state/codex-plugin" ]] &&
        jq -n --arg root "$root" '{
          installed:[{pluginId:"context-mode@context-mode",version:"1.0.169",
            installed:true,enabled:true,source:{source:"local",path:$root},
            marketplaceSource:{sourceType:"local",source:$root}}],
          available:[]
        }' ||
        printf '{"installed":[],"available":[]}\n'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == add ]]; then
    mkdir -p "$state"; : > "$state/codex-plugin"
    cache_root="${CODEX_HOME:-$HOME/.codex}/plugins/cache/context-mode/context-mode/1.0.169"
    mkdir -p "$cache_root"
    cp -a -- "$root/." "$cache_root/"
    printf 'CODEX_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    printf '{"ok":true}\n'
    exit
fi
if [[ "${1:-}" == plugin && "${2:-}" == remove ]]; then
    rm -f -- "$state/codex-plugin"
    printf 'CODEX_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    printf '{"ok":true}\n'
    exit
fi
if [[ "${1:-}" == features && "${2:-}" == list ]]; then
    [[ -e "$state/codex-hooks" ||
       "${FAKE_CONTEXT_HOOKS_DEFAULT_TRUE:-0}" == 1 ]] &&
        printf 'hooks stable true\n' ||
        printf 'hooks stable false\n'
    printf 'plugin_hooks removed false\n'
    exit
fi
if [[ "${1:-}" == features && "${2:-}" == enable && "${3:-}" == hooks ]]; then
    config_home="${CODEX_HOME:-$HOME/.codex}"
    config_file="$config_home/config.toml"
    mkdir -p "$config_home"
    if [[ ! -f "$config_file" ]] ||
       ! grep -Fqx 'hooks = true' "$config_file"; then
        if [[ -s "$config_file" ]]; then
            printf '\n' >> "$config_file"
        fi
        printf '[features]\nhooks = true\n' >> "$config_file"
    fi
    if [[ "${AGS_CONTEXT_MODE_CONFIG_PROBE:-0}" == 1 ]]; then
        exit
    fi
    mkdir -p "$state"; : > "$state/codex-hooks"
    printf 'CODEX_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
if [[ "${1:-}" == features && "${2:-}" == disable && "${3:-}" == hooks ]]; then
    rm -f -- "$state/codex-hooks"
    printf 'CODEX_CONTEXT=%s\n' "$*" >> "$HOME/.context-mode-test.log"
    exit
fi
exit 64
EOF
chmod +x "$offline_guard_bin/codex"
printf '{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"keep-me"}]}]}}\n' \
    > "$home/.codex/hooks.json"
printf '{"existing":true}\n' > "$home/.claude/settings.json"

run_installer() {
    env HOME="$home" \
        XDG_CONFIG_HOME="$home/.config" \
        XDG_DATA_HOME="$home/.local/share" \
        XDG_STATE_HOME="$home/.local/state" \
        PATH="$offline_guard_bin:$bin_dir:$PATH" \
        FAKE_CONTEXT_HOOKS_DEFAULT_TRUE=1 \
        OFFLINE_NETWORK_MARKER="$offline_network_marker" \
        VERSION=v0.3.0-ags.1 \
        "$project_root/install.sh" --offline "$artifact" --dest "$bin_dir" \
        --no-verify --quiet
}

if ! run_installer > "$tmp/first.out" 2> "$tmp/first.err"; then
    printf 'initial offline installer run failed:\n' >&2
    cat "$tmp/first.err" >&2
    exit 1
fi
if grep -Fq 'AGE-SECRET-KEY-' "$tmp/first.out" "$tmp/first.err"; then
    printf 'installer printed the secret identity\n' >&2
    exit 1
fi
[[ ! -e "$offline_network_marker" ]]

identity="$home/.config/ags/identity.agekey"
config="$home/.local/state/ags/storage.json"
vault="$home/.local/share/ags/checkpoints"
[[ -x "$bin_dir/casr" && -x "$bin_dir/ags" ]]
[[ -x "$bin_dir/rmux" && -x "$bin_dir/rmux-daemon" ]]
[[ -x "$tmp/libexec/rmux/rmux" ]]
[[ "$("$bin_dir/rmux" -V)" == 'rmux 0.9.1' ]]
[[ ! -e "$bin_dir/.rmux-install-transaction.json" ]]
[[ -z "$(find "$bin_dir" "$tmp/libexec/rmux" -type f \
    \( -name '.rmux.rollback.*' -o -name '*.install-transaction.*' \) \
    -print -quit)" ]]
[[ -f "$identity" && -f "$config" && -d "$vault" ]]
[[ "$(stat -c %a "$identity")" == 600 ]]
[[ "$(stat -c %a "$config")" == 600 ]]
jq -e --arg vault "$vault" --arg identity "$identity" '
    .version == 4 and .local_path == $vault and
    .encryption == {type:"age-x25519", identity_file:$identity}
' "$config" >/dev/null
jq -e --arg root "$context_root" \
    --arg platform "$FAKE_CONTEXT_RUNTIME_PLATFORM" \
    --arg arch "$FAKE_CONTEXT_RUNTIME_ARCH" \
    --argjson node_abi "$FAKE_CONTEXT_RUNTIME_NODE_ABI" \
    --arg target "$FAKE_CONTEXT_RUNTIME_TARGET" \
    --arg files_sha256 "$(
        test_sha256_file "$context_runtime/ags-files.sha256"
    )" '
    .schema == 2 and .version == "1.0.169" and .package_root == $root and
    .runtime == {
      platform:$platform,arch:$arch,node_abi:$node_abi,target:$target
    } and
    .source.files_sha256 == $files_sha256 and
    .health == {mode:"offline-index-search",status:"passed"} and
    .providers.claude.configured == true and
    .providers.codex.configured == true and
    .providers.codex.trust == "official-review-required"
' "$home/.local/state/ags/context-mode.json" >/dev/null

for root in "$home/.codex/skills" "$home/.claude/skills"; do
    cmp "$project_root/plugins/ags/skills/ags/SKILL.md" "$root/ags/SKILL.md"
    cmp "$project_root/plugins/ags/skills/ags/agents/openai.yaml" \
        "$root/ags/agents/openai.yaml"
done
hook_command="$bin_dir/casr checkpoint hook"
jq -e --arg command "$hook_command" '
    any(.hooks.Stop[]?.hooks[]?; .command == "keep-me") and
    any(.hooks.Stop[]?.hooks[]?; .command == $command) and
    any(.hooks.SessionStart[]?.hooks[]?; .command == $command)
' "$home/.codex/hooks.json" >/dev/null
jq -e --arg command "$hook_command" '
    .existing == true and .skillOverrides.ags == "user-invocable-only" and
    any(.hooks.Stop[]?.hooks[]?; .command == $command) and
    any(.hooks.SessionStart[]?.hooks[]?; .command == $command)
' "$home/.claude/settings.json" >/dev/null

runtime_env=(
    env
    HOME="$home"
    XDG_CONFIG_HOME="$home/.config"
    XDG_DATA_HOME="$home/.local/share"
    XDG_STATE_HOME="$home/.local/state"
    PATH="$bin_dir:$PATH"
)
"${runtime_env[@]}" "$bin_dir/ags" --help | grep -Fq 'Usage:'
"${runtime_env[@]}" "$bin_dir/casr" checkpoint list >/dev/null

checkpoint_work="$tmp/checkpoint-work"
checkpoint_codex_home="$tmp/checkpoint-codex-home"
checkpoint_session_id=12345678-1234-4234-8234-123456789abc
checkpoint_rollout="$checkpoint_codex_home/sessions/2026/07/29/rollout-smoke-$checkpoint_session_id.jsonl"
mkdir -p "$checkpoint_work" "${checkpoint_rollout%/*}"
printf '%s\n' \
    "{\"timestamp\":\"2026-07-29T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"$checkpoint_session_id\",\"cwd\":\"$checkpoint_work\",\"model_provider\":\"openai\"}}" \
    '{"timestamp":"2026-07-29T00:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"installer hook smoke"}}' \
    > "$checkpoint_rollout"
(
    cd "$checkpoint_work"
    "${runtime_env[@]}" CODEX_HOME="$checkpoint_codex_home" \
        AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$checkpoint_session_id" \
        "$bin_dir/ags" save installer-hook 'Installer hook smoke'
) >/dev/null
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$checkpoint_session_id" |
    "${runtime_env[@]}" CODEX_HOME="$checkpoint_codex_home" \
        "$bin_dir/casr" checkpoint hook
find "$vault/codex" -maxdepth 1 -type f \
    -name '*@installer-hook.checkpoint.tar.gz.age' | grep -q .
[[ -z "$(find "$home/.local/state/ags/pending" -type f -name '*.json' -print -quit)" ]]

identity_sha="$(sha256sum "$identity" | cut -d' ' -f1)"
custom_vault="$tmp/custom-vault"
"${runtime_env[@]}" "$bin_dir/ags" set "$custom_vault" >/dev/null
if ! run_installer > "$tmp/second.out" 2> "$tmp/second.err"; then
    printf 'repeat offline installer run failed:\n' >&2
    cat "$tmp/second.err" >&2
    exit 1
fi
[[ "$identity_sha" == "$(sha256sum "$identity" | cut -d' ' -f1)" ]]
jq -e --arg vault "$custom_vault" '.local_path == $vault' "$config" >/dev/null
[[ "$(grep -c '^CLAUDE_CONTEXT=plugin marketplace add ' \
    "$home/.context-mode-test.log")" == 1 ]]
[[ "$(grep -c '^CLAUDE_CONTEXT=plugin install ' \
    "$home/.context-mode-test.log")" == 1 ]]
[[ "$(grep -c '^CODEX_CONTEXT=plugin marketplace add ' \
    "$home/.context-mode-test.log")" == 1 ]]
[[ "$(grep -c '^CODEX_CONTEXT=plugin add ' \
    "$home/.context-mode-test.log")" == 1 ]]

rollback_home="$tmp/rollback-home"
rollback_bin="$tmp/rollback-bin"
rollback_state="$rollback_home/.local/state/context-mode-fake"
mkdir -p "$rollback_home" "$rollback_bin"
rollback_context_sha="$(
    test_sha256_file "$rollback_home/.local/state/ags/context-mode.json"
)"
printf '%s\n' '#!/bin/sh' 'printf "preexisting-casr\n"' \
    > "$rollback_bin/casr"
chmod +x "$rollback_bin/casr"
rollback_binary_sha="$(sha256sum "$rollback_bin/casr" | cut -d' ' -f1)"
if env HOME="$rollback_home" \
    XDG_CONFIG_HOME="$rollback_home/.config" \
    XDG_DATA_HOME="$rollback_home/.local/share" \
    XDG_STATE_HOME="$rollback_home/.local/state" \
    PATH="$offline_guard_bin:$rollback_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    FAKE_CONTEXT_HEALTH_FAIL=codex \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$rollback_bin" \
    --no-verify --quiet > "$tmp/rollback.out" 2> "$tmp/rollback.err"; then
    printf 'installer ignored a mandatory Context Mode health failure\n' >&2
    exit 1
fi
[[ "$rollback_binary_sha" == "$(sha256sum "$rollback_bin/casr" | cut -d' ' -f1)" ]]
[[ "$("$rollback_bin/casr")" == preexisting-casr ]]
[[ ! -e "$rollback_bin/rmux" && ! -e "$rollback_bin/rmux-daemon" ]]
[[ ! -e "$rollback_bin/libexec/rmux/rmux" ]]
[[ ! -e "$rollback_home/.codex/config.toml" ]]
[[ ! -e "$rollback_home/.config/ags/identity.agekey" ]]
[[ ! -e "$rollback_home/.local/state/ags/storage.json" ]]
[[ ! -e "$rollback_bin/.casr.install-transaction.json" ]]
[[ ! -e "$rollback_bin/.rmux-install-transaction.json" ]]
[[ -z "$(find "$rollback_bin" -type f \
    \( -name '.rmux.rollback.*' -o -name '*.install-transaction.*' \) \
    -print -quit)" ]]

interrupted_home="$tmp/interrupted-home"
interrupted_bin="$tmp/interrupted-bin"
interrupted_backup="$interrupted_bin/.casr.rollback.recovery"
interrupted_stage="$interrupted_bin/.casr.install.recovery"
interrupted_journal="$interrupted_bin/.casr.install-transaction.json"
interrupted_missing_artifact="$tmp/interrupted-missing.tar.xz"
mkdir -p "$interrupted_home" "$interrupted_bin"
printf '%s\n' '#!/bin/sh' 'printf "recovered-casr\n"' \
    > "$interrupted_backup"
chmod 755 "$interrupted_backup"
interrupted_previous_sha="$(test_sha256_file "$interrupted_backup")"
interrupted_candidate_sha="$(test_sha256_file "$binary")"
interrupted_context_sha="$(
    test_sha256_file \
        "$interrupted_home/.local/state/ags/context-mode.json"
)"
jq -n \
    --arg binary "$interrupted_bin/casr" \
    --arg candidate_sha "$interrupted_candidate_sha" \
    --arg stage "$interrupted_stage" \
    --arg previous_sha "$interrupted_previous_sha" \
    --arg backup "$interrupted_backup" \
    --arg active "$interrupted_home/.local/state/ags/context-mode.json" \
    --arg pending "$interrupted_home/.local/state/ags/context-mode.pending.json" \
    --arg active_before "$interrupted_context_sha" '{
      schema:1,
      managed_by:"ags-installer",
      binary_path:$binary,
      candidate:{sha256:$candidate_sha,stage_path:$stage},
      previous:{
        existed:true,
        sha256:$previous_sha,
        backup_path:$backup
      },
      context:{
        active_manifest_path:$active,
        pending_manifest_path:$pending,
        active_before:$active_before
      }
    }' > "$interrupted_journal"
chmod 600 "$interrupted_journal"
if env HOME="$interrupted_home" \
    XDG_CONFIG_HOME="$interrupted_home/.config" \
    XDG_DATA_HOME="$interrupted_home/.local/share" \
    XDG_STATE_HOME="$interrupted_home/.local/state" \
    PATH="$offline_guard_bin:$interrupted_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$interrupted_missing_artifact" \
    --dest "$interrupted_bin" --no-verify \
    > "$tmp/interrupted.out" 2> "$tmp/interrupted.err"; then
    printf 'installer unexpectedly accepted a missing recovery artifact\n' >&2
    exit 1
fi
if ! grep -Fq 'Restored the previous binary after interrupted activation' \
    "$tmp/interrupted.out" "$tmp/interrupted.err"; then
    printf 'interrupted binary recovery did not report rollback:\n' >&2
    cat "$tmp/interrupted.out" >&2
    cat "$tmp/interrupted.err" >&2
    exit 1
fi
grep -Fq 'Offline tarball not found' "$tmp/interrupted.err"
[[ "$("$interrupted_bin/casr")" == recovered-casr ]]
[[ "$interrupted_previous_sha" == "$(
    test_sha256_file "$interrupted_bin/casr"
)" ]]
[[ ! -e "$interrupted_journal" && ! -e "$interrupted_backup" ]]

rmux_partial_home="$tmp/rmux-partial-home"
rmux_partial_bin="$tmp/rmux-partial-bin"
rmux_partial_helper="$rmux_partial_bin/libexec/rmux/rmux"
rmux_partial_journal="$rmux_partial_bin/.rmux-install-transaction.json"
rmux_partial_missing="$tmp/rmux-partial-missing.tar.xz"
mkdir -p "$rmux_partial_home" "$(dirname "$rmux_partial_helper")"
rmux_partial_destinations=(
    "$rmux_partial_bin/rmux"
    "$rmux_partial_bin/rmux-daemon"
    "$rmux_partial_helper"
)
rmux_partial_sources=(
    "$artifact_root/bin/rmux"
    "$artifact_root/bin/rmux-daemon"
    "$artifact_root/libexec/rmux/rmux"
)
rmux_partial_previous_shas=()
rmux_partial_candidate_shas=()
for index in 0 1 2; do
    backup="$rmux_partial_bin/.rmux.rollback.$index.partial"
    stage="${rmux_partial_destinations[$index]}.install-transaction.partial"
    printf '#!/bin/sh\nprintf "previous-rmux-%s\\n"\n' "$index" > "$backup"
    chmod 755 "$backup"
    cp -p -- "$backup" "${rmux_partial_destinations[$index]}"
    cp -p -- "${rmux_partial_sources[$index]}" "$stage"
    rmux_partial_previous_shas[$index]="$(test_sha256_file "$backup")"
    rmux_partial_candidate_shas[$index]="$(test_sha256_file "$stage")"
done
mv -f -- \
    "${rmux_partial_destinations[0]}.install-transaction.partial" \
    "${rmux_partial_destinations[0]}"
mv -f -- \
    "${rmux_partial_destinations[1]}.install-transaction.partial" \
    "${rmux_partial_destinations[1]}"
write_rmux_recovery_journal \
    "$rmux_partial_journal" "$rmux_partial_bin" "$rmux_partial_bin" \
    "${rmux_partial_candidate_shas[0]}" \
    "${rmux_partial_candidate_shas[1]}" \
    "${rmux_partial_candidate_shas[2]}" \
    "${rmux_partial_previous_shas[0]}" \
    "${rmux_partial_previous_shas[1]}" \
    "${rmux_partial_previous_shas[2]}" \
    partial
if env HOME="$rmux_partial_home" \
    XDG_CONFIG_HOME="$rmux_partial_home/.config" \
    XDG_DATA_HOME="$rmux_partial_home/.local/share" \
    XDG_STATE_HOME="$rmux_partial_home/.local/state" \
    PATH="$offline_guard_bin:$rmux_partial_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$rmux_partial_missing" \
    --dest "$rmux_partial_bin" --no-verify \
    > "$tmp/rmux-partial.out" 2> "$tmp/rmux-partial.err"; then
    printf 'installer unexpectedly accepted a missing RMUX recovery artifact\n' >&2
    exit 1
fi
grep -Fq 'Rolled back an interrupted RMUX activation' \
    "$tmp/rmux-partial.out" "$tmp/rmux-partial.err"
grep -Fq 'Offline tarball not found' "$tmp/rmux-partial.err"
for index in 0 1 2; do
    [[ "$(test_sha256_file "${rmux_partial_destinations[$index]}")" == \
       "${rmux_partial_previous_shas[$index]}" ]]
    [[ ! -e "$rmux_partial_bin/.rmux.rollback.$index.partial" ]]
    [[ ! -e "${rmux_partial_destinations[$index]}.install-transaction.partial" ]]
done
[[ ! -e "$rmux_partial_journal" ]]

rmux_resume_home="$tmp/rmux-resume-home"
rmux_resume_bin="$tmp/rmux-resume-bin"
rmux_resume_helper="$rmux_resume_bin/libexec/rmux/rmux"
rmux_resume_journal="$rmux_resume_bin/.rmux-install-transaction.json"
rmux_resume_binary_journal="$rmux_resume_bin/.casr.install-transaction.json"
rmux_resume_binary_backup="$rmux_resume_bin/.casr.rollback.recovery"
rmux_resume_binary_stage="$rmux_resume_bin/.casr.install.recovery"
rmux_resume_missing="$tmp/rmux-resume-missing.tar.xz"
mkdir -p "$rmux_resume_home" "$(dirname "$rmux_resume_helper")"
rmux_resume_destinations=(
    "$rmux_resume_bin/rmux"
    "$rmux_resume_bin/rmux-daemon"
    "$rmux_resume_helper"
)
rmux_resume_sources=(
    "$artifact_root/bin/rmux"
    "$artifact_root/bin/rmux-daemon"
    "$artifact_root/libexec/rmux/rmux"
)
rmux_resume_previous_shas=()
rmux_resume_candidate_shas=()
for index in 0 1 2; do
    backup="$rmux_resume_bin/.rmux.rollback.$index.recovery"
    printf '#!/bin/sh\nprintf "previous-rmux-%s\\n"\n' "$index" > "$backup"
    chmod 755 "$backup"
    install -m 0755 \
        "${rmux_resume_sources[$index]}" \
        "${rmux_resume_destinations[$index]}"
    rmux_resume_previous_shas[$index]="$(test_sha256_file "$backup")"
    rmux_resume_candidate_shas[$index]="$(
        test_sha256_file "${rmux_resume_destinations[$index]}"
    )"
done
write_rmux_recovery_journal \
    "$rmux_resume_journal" "$rmux_resume_bin" "$rmux_resume_bin" \
    "${rmux_resume_candidate_shas[0]}" \
    "${rmux_resume_candidate_shas[1]}" \
    "${rmux_resume_candidate_shas[2]}" \
    "${rmux_resume_previous_shas[0]}" \
    "${rmux_resume_previous_shas[1]}" \
    "${rmux_resume_previous_shas[2]}" \
    recovery
printf '%s\n' '#!/bin/sh' 'printf "previous-casr\n"' \
    > "$rmux_resume_binary_backup"
chmod 755 "$rmux_resume_binary_backup"
install -m 0755 "$binary" "$rmux_resume_bin/casr"
rmux_resume_binary_candidate_sha="$(
    test_sha256_file "$rmux_resume_bin/casr"
)"
rmux_resume_binary_previous_sha="$(
    test_sha256_file "$rmux_resume_binary_backup"
)"
jq -n \
    --arg binary "$rmux_resume_bin/casr" \
    --arg candidate_sha "$rmux_resume_binary_candidate_sha" \
    --arg stage "$rmux_resume_binary_stage" \
    --arg previous_sha "$rmux_resume_binary_previous_sha" \
    --arg backup "$rmux_resume_binary_backup" \
    --arg active "$rmux_resume_home/.local/state/ags/context-mode.json" \
    --arg pending "$rmux_resume_home/.local/state/ags/context-mode.pending.json" \
    --arg active_before \
        0000000000000000000000000000000000000000000000000000000000000000 '{
      schema:1,
      managed_by:"ags-installer",
      binary_path:$binary,
      candidate:{sha256:$candidate_sha,stage_path:$stage},
      previous:{
        existed:true,
        sha256:$previous_sha,
        backup_path:$backup
      },
      context:{
        active_manifest_path:$active,
        pending_manifest_path:$pending,
        active_before:$active_before
      }
    }' > "$rmux_resume_binary_journal"
chmod 600 "$rmux_resume_binary_journal"
env HOME="$rmux_resume_home" \
    XDG_CONFIG_HOME="$rmux_resume_home/.config" \
    XDG_DATA_HOME="$rmux_resume_home/.local/share" \
    XDG_STATE_HOME="$rmux_resume_home/.local/state" \
    PATH="$offline_guard_bin:$rmux_resume_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$rmux_resume_missing" \
    --dest "$rmux_resume_bin" --no-verify \
    > "$tmp/rmux-resume.out" 2> "$tmp/rmux-resume.err"
grep -Fq 'Recovering interrupted RMUX installation' \
    "$tmp/rmux-resume.out" "$tmp/rmux-resume.err"
grep -Fq 'Resuming the interrupted casr installation' \
    "$tmp/rmux-resume.out" "$tmp/rmux-resume.err"
grep -Fq 'Finishing the recovered casr transaction' \
    "$tmp/rmux-resume.out" "$tmp/rmux-resume.err"
[[ ! -e "$rmux_resume_missing" && ! -e "$offline_network_marker" ]]
[[ "$(test_sha256_file "$rmux_resume_bin/casr")" == \
   "$rmux_resume_binary_candidate_sha" ]]
for index in 0 1 2; do
    [[ "$(test_sha256_file "${rmux_resume_destinations[$index]}")" == \
       "${rmux_resume_candidate_shas[$index]}" ]]
    [[ ! -e "$rmux_resume_bin/.rmux.rollback.$index.recovery" ]]
    [[ ! -e "${rmux_resume_destinations[$index]}.install-transaction.recovery" ]]
done
[[ "$("$rmux_resume_bin/rmux" -V)" == 'rmux 0.9.1' ]]
[[ ! -e "$rmux_resume_journal" ]]
[[ ! -e "$rmux_resume_binary_journal" ]]
[[ ! -e "$rmux_resume_binary_backup" && ! -e "$rmux_resume_binary_stage" ]]
jq -e '
    .health == {mode:"offline-index-search",status:"passed"} and
    .providers.claude.configured == true and
    .providers.codex.configured == true
' "$rmux_resume_home/.local/state/ags/context-mode.json" >/dev/null

new_failure_home="$tmp/new-failure-home"
new_failure_bin="$tmp/new-failure-bin"
mkdir -p "$new_failure_home" "$new_failure_bin"
new_failure_context_sha="$(
    test_sha256_file "$new_failure_home/.local/state/ags/context-mode.json"
)"
if env HOME="$new_failure_home" \
    XDG_CONFIG_HOME="$new_failure_home/.config" \
    XDG_DATA_HOME="$new_failure_home/.local/share" \
    XDG_STATE_HOME="$new_failure_home/.local/state" \
    PATH="$offline_guard_bin:$new_failure_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    FAKE_CONTEXT_HEALTH_FAIL=codex \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$new_failure_bin" \
    --no-verify --quiet > "$tmp/new-failure.out" \
    2> "$tmp/new-failure.err"; then
    printf 'new install ignored a mandatory Context Mode health failure\n' >&2
    exit 1
fi
[[ ! -e "$new_failure_bin/casr" ]]
[[ ! -e "$new_failure_bin/rmux" && ! -e "$new_failure_bin/rmux-daemon" ]]
[[ ! -e "$new_failure_bin/libexec/rmux/rmux" ]]
[[ "$new_failure_context_sha" == "$(
    test_sha256_file "$new_failure_home/.local/state/ags/context-mode.json"
)" ]]
[[ ! -e "$new_failure_bin/.casr.install-transaction.json" ]]
[[ ! -e "$new_failure_bin/.rmux-install-transaction.json" ]]
[[ -z "$(find "$new_failure_bin" -type f \
    \( -name '.rmux.rollback.*' -o -name '*.install-transaction.*' \) \
    -print -quit)" ]]

unmanaged_home="$tmp/unmanaged-home"
unmanaged_bin="$tmp/unmanaged-bin"
mkdir -p "$unmanaged_home" "$unmanaged_bin"
printf 'unmanaged\n' > "$unmanaged_bin/ags"
env HOME="$unmanaged_home" \
    XDG_CONFIG_HOME="$unmanaged_home/.config" \
    XDG_DATA_HOME="$unmanaged_home/.local/share" \
    XDG_STATE_HOME="$unmanaged_home/.local/state" \
    PATH="$offline_guard_bin:$unmanaged_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$unmanaged_bin" \
    --no-verify --quiet > "$tmp/unmanaged.out" 2> "$tmp/unmanaged.err"
grep -Fqx 'unmanaged' "$unmanaged_bin/ags"

symlink_home="$tmp/symlink-home"
symlink_bin="$tmp/symlink-bin"
symlink_tools="$tmp/symlink-tools"
skill_outside="$tmp/skill-outside"
hook_outside="$tmp/hook-outside.json"
mkdir -p "$symlink_home/.codex/skills" "$symlink_home/.claude" \
    "$skill_outside" "$symlink_tools"
ln -s "$offline_guard_bin/node" "$symlink_tools/node"
printf '{"outside":true}\n' > "$hook_outside"
ln -s "$skill_outside" "$symlink_home/.codex/skills/ags"
ln -s "$hook_outside" "$symlink_home/.claude/settings.json"
env HOME="$symlink_home" \
    XDG_CONFIG_HOME="$symlink_home/.config" \
    XDG_DATA_HOME="$symlink_home/.local/share" \
    XDG_STATE_HOME="$symlink_home/.local/state" \
    PATH="$symlink_tools:$symlink_bin:/usr/bin:/bin" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-ags.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$symlink_bin" \
    --no-verify > "$tmp/symlink.out" 2> "$tmp/symlink.err"
[[ ! -e "$skill_outside/SKILL.md" ]]
grep -Fqx '{"outside":true}' "$hook_outside"
grep -Fq 'Checkpoint skill path contains a symbolic link' "$tmp/symlink.out"
grep -Fq 'Checkpoint hook path contains a symbolic link' "$tmp/symlink.out"

online_home="$tmp/online-home"
online_bin="$tmp/online-bin"
online_tools="$tmp/online-tools"
online_fixture_home="$tmp/online-context-fixture"
online_npm_log="$tmp/online-npm.log"
mkdir -p "$online_home" "$online_bin" "$online_tools"
mkdir -p "$online_bin/libexec/rmux"
install -m 0755 "$artifact_root/bin/rmux" "$online_bin/rmux"
install -m 0755 "$artifact_root/bin/rmux-daemon" "$online_bin/rmux-daemon"
install -m 0755 "$artifact_root/libexec/rmux/rmux" \
    "$online_bin/libexec/rmux/rmux"

if (( EUID == 0 )); then
    if env HOME="$tmp/system-root-home" \
        XDG_CONFIG_HOME="$tmp/system-root-home/.config" \
        XDG_DATA_HOME="$tmp/system-root-home/.local/share" \
        XDG_STATE_HOME="$tmp/system-root-home/.local/state" \
        "$project_root/install.sh" --system --offline "$artifact" \
        --no-verify --quiet > "$tmp/system-root.out" \
        2> "$tmp/system-root.err"; then
        printf 'installer accepted root --system for a per-user Context Mode setup\n' >&2
        exit 1
    fi
    grep -Fq -- '--system cannot run as root' "$tmp/system-root.err"
fi

# A wrapper written before the agsx name was dropped must still be recognised as
# the installer's own. `# ags-installer-` is not a substring of
# `# agsx-installer-`, so a single-marker check reads those machines as
# hand-written, preserves the file, and never updates it again — which would
# strand every installation predating that rename.
cat > "$bin_dir/ags" <<'EOF'
#!/bin/sh
# agsx-installer-checkpoint-wrapper
script_dir=$(CDPATH= cd -P -- "$(dirname -- "$0")" && pwd)
exec "$script_dir/casr" checkpoint "$@"
EOF
chmod 0755 "$bin_dir/ags"
if ! run_installer > "$tmp/legacy-wrapper.out" 2> "$tmp/legacy-wrapper.err"; then
    printf 'installer run over a pre-rename wrapper failed:\n' >&2
    cat "$tmp/legacy-wrapper.err" >&2
    exit 1
fi
if grep -Fq 'agsx-installer-checkpoint-wrapper' "$bin_dir/ags"; then
    printf 'installer left a pre-rename wrapper in place instead of adopting it\n' >&2
    exit 1
fi
grep -Fq '# ags-installer-checkpoint-wrapper' "$bin_dir/ags"

# A wrapper the installer did not write is still left alone.
printf '#!/bin/sh\n# mine\nexec true\n' > "$bin_dir/ags"
chmod 0755 "$bin_dir/ags"
if ! run_installer > "$tmp/unmanaged-wrapper.out" 2> "$tmp/unmanaged-wrapper.err"; then
    printf 'installer run over an unmanaged wrapper failed:\n' >&2
    cat "$tmp/unmanaged-wrapper.err" >&2
    exit 1
fi
grep -Fqx '# mine' "$bin_dir/ags"

printf 'ags install smoke passed (%s/%s)\n' "$platform" "$(uname -m)"
