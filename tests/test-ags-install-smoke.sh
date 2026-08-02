#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_target="${CARGO_TARGET_DIR:-$project_root/target}"
binary="${CASR_TEST_BINARY:-$cargo_target/debug/casr}"
platform="$(uname -s)"
test_tmp_root=/tmp
[[ "$platform" != Darwin ]] || test_tmp_root=/private/tmp
tmp="$(mktemp -d "$test_tmp_root/agsx-install-smoke.XXXXXX")"
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
        /tmp/agsx-install-smoke.*|/private/tmp/agsx-install-smoke.*) rm -rf -- "$tmp" ;;
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

context_mode_test_runtime_root() {
    local runtime_home="$1" version="$2"
    printf '%s/.local/share/ags/context-mode/runtimes/%s/%s\n' \
        "$runtime_home" "$FAKE_CONTEXT_RUNTIME_TARGET" "$version"
}

write_context_mode_test_manifest() {
    local runtime_root="$1" package_root="$runtime_root/node_modules/context-mode"
    local file
    {
        printf 'ags-context-tree-v1\n'
        while IFS= read -r -d '' file; do
            printf 'f\t%s\t%s\n' \
                "$(test_sha256_file "$file")" "${file#"$package_root/"}"
        done < <(
            find "$package_root" -type f -print0 | LC_ALL=C sort -z
        )
    } > "$runtime_root/ags-files.sha256"
    chmod 600 "$runtime_root/ags-files.sha256"
    mkdir -p "$runtime_root/ags-pristine"
    cp -p -- "$package_root/.claude-plugin/plugin.json" \
        "$runtime_root/ags-pristine/claude-plugin.json"
    cp -p -- "$package_root/hooks/hooks.json" \
        "$runtime_root/ags-pristine/claude-hooks.json"
    chmod 600 "$runtime_root/ags-pristine/claude-plugin.json" \
        "$runtime_root/ags-pristine/claude-hooks.json"
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

prepare_context_mode_runtime() {
    local runtime_home="$1"
    local runtime_root
    runtime_root="$(context_mode_test_runtime_root "$runtime_home" 1.0.169)"
    local package_root="$runtime_root/node_modules/context-mode"
    local hook node_abi
    node_abi="$("$FAKE_REAL_NODE_BINARY" -p 'process.versions.modules')"
    mkdir -p "$package_root/.claude-plugin" "$package_root/.codex-plugin" \
        "$package_root/hooks/codex" "$package_root/scripts" \
        "$package_root/node_modules/better-sqlite3/build/Release" \
        "$package_root/node_modules/@modelcontextprotocol/sdk"
    jq -n '{
      lockfileVersion:3,
      packages:{
        "node_modules/context-mode":{
          version:"1.0.169",
          resolved:"https://registry.npmjs.org/context-mode/-/context-mode-1.0.169.tgz",
          integrity:"sha512-94JIaFuLjF9SO2BsGTrbGtyT44K95+9OC8BdbaL/UT76xOkanJLfUR5CzmNw+GELXZQqH4nBrKg9wjBnSFkVnQ=="
        }
      }
    }' > "$runtime_root/package-lock.json"
    printf '%s\n' \
        '{"name":"context-mode","version":"1.0.169","license":"Elastic-2.0","dependencies":{"better-sqlite3":"test","@modelcontextprotocol/sdk":"test"}}' \
        > "$package_root/package.json"
    jq -n '{
      name:"context-mode",
      metadata:{version:"1.0.169"},
      plugins:[{name:"context-mode",source:"./",version:"1.0.169"}]
    }' > "$package_root/.claude-plugin/marketplace.json"
    jq -n '{
      name:"context-mode",version:"1.0.169",skills:"./skills/",
      mcpServers:{"context-mode":{
        command:"node",args:["${CLAUDE_PLUGIN_ROOT}/start.mjs"]
      }}
    }' > "$package_root/.claude-plugin/plugin.json"
    jq -n '{
      name:"context-mode",version:"1.0.169",skills:"./skills/",
      mcpServers:"./.codex-plugin/mcp.json",
      hooks:"./.codex-plugin/hooks.json"
    }' > "$package_root/.codex-plugin/plugin.json"
    jq -n '{
      mcpServers:{"context-mode":{
        command:"node",args:["./start.mjs"],cwd:".",
        env:{CONTEXT_MODE_PLATFORM:"codex"}
      }}
    }' > "$package_root/.codex-plugin/mcp.json"
    jq -n '{
      hooks:{
        PreToolUse:[{
          matcher:"local_shell|shell|shell_command|exec_command|Bash|Shell|apply_patch|Edit|Write|grep_files|ctx_execute|ctx_execute_file|ctx_batch_execute|ctx_fetch_and_index|ctx_search|ctx_index|mcp__",
          hooks:[{type:"command",command:"node \"${PLUGIN_ROOT}/hooks/codex/pretooluse.mjs\""}]
        }],
        PostToolUse:[{hooks:[{type:"command",command:"node \"${PLUGIN_ROOT}/hooks/codex/posttooluse.mjs\""}]}],
        SessionStart:[{hooks:[{type:"command",command:"node \"${PLUGIN_ROOT}/hooks/codex/sessionstart.mjs\""}]}],
        PreCompact:[{hooks:[{type:"command",command:"node \"${PLUGIN_ROOT}/hooks/codex/precompact.mjs\""}]}],
        UserPromptSubmit:[{hooks:[{type:"command",command:"node \"${PLUGIN_ROOT}/hooks/codex/userpromptsubmit.mjs\""}]}],
        Stop:[{hooks:[{type:"command",command:"node \"${PLUGIN_ROOT}/hooks/codex/stop.mjs\""}]}]
      }
    }' > "$package_root/.codex-plugin/hooks.json"
    jq -n '{
      description:"Context Mode test hooks",
      hooks:{
        PreToolUse:(
          ["Bash","WebFetch","Read","Grep","Agent","mcp__"] |
          map(. as $matcher | {
            matcher:$matcher,
            hooks:[{
              type:"command",
              command:"node \"${CLAUDE_PLUGIN_ROOT}/hooks/pretooluse.mjs\""
            }]
          })
        ),
        PostToolUse:[{matcher:"",hooks:[{
          type:"command",
          command:"node \"${CLAUDE_PLUGIN_ROOT}/hooks/posttooluse.mjs\""
        }]}],
        SessionStart:[{matcher:"",hooks:[{
          type:"command",
          command:"node \"${CLAUDE_PLUGIN_ROOT}/hooks/sessionstart.mjs\""
        }]}],
        PreCompact:[{matcher:"",hooks:[{
          type:"command",
          command:"node \"${CLAUDE_PLUGIN_ROOT}/hooks/precompact.mjs\""
        }]}],
        UserPromptSubmit:[{matcher:"",hooks:[{
          type:"command",
          command:"node \"${CLAUDE_PLUGIN_ROOT}/hooks/userpromptsubmit.mjs\""
        }]}],
        Stop:[{matcher:"",hooks:[{
          type:"command",
          command:"node \"${CLAUDE_PLUGIN_ROOT}/hooks/stop.mjs\""
        }]}]
      }
    }' > "$package_root/hooks/hooks.json"
    printf '// context-mode offline test entrypoint\n' > "$package_root/cli.bundle.mjs"
    printf '// context-mode offline test MCP entrypoint\n' > "$package_root/start.mjs"
    printf '// context-mode offline test server\n' > "$package_root/server.bundle.mjs"
    for hook in pretooluse posttooluse sessionstart precompact userpromptsubmit stop; do
        printf '// context-mode claude %s hook\n' "$hook" \
            > "$package_root/hooks/$hook.mjs"
        printf '// context-mode codex %s hook\n' "$hook" \
            > "$package_root/hooks/codex/$hook.mjs"
    done
    printf '// context-mode codex platform bridge\n' \
        > "$package_root/hooks/codex/platform.mjs"
    printf '// context-mode dependency provision test\n' \
        > "$package_root/hooks/ensure-deps.mjs"
    printf '// context-mode native healing test\n' \
        > "$package_root/scripts/heal-better-sqlite3.mjs"
    cat > "$package_root/node_modules/better-sqlite3/package.json" <<'EOF'
{"name":"better-sqlite3","version":"0.0.0-test","main":"index.js","dependencies":{}}
EOF
    cat > "$package_root/node_modules/better-sqlite3/index.js" <<'EOF'
class TestDatabase {
  constructor() {}
  exec(statement) {
    if (!statement.includes("fts5")) throw new Error("expected FTS5 probe");
  }
  close() {}
}
module.exports = TestDatabase;
EOF
    cat > "$package_root/node_modules/@modelcontextprotocol/sdk/package.json" <<'EOF'
{"name":"@modelcontextprotocol/sdk","version":"0.0.0-test","exports":{".":{"import":"./dist/esm/index.js","require":"./dist/cjs/index.js"}},"dependencies":{}}
EOF
    printf 'test native binding\n' \
        > "$package_root/node_modules/better-sqlite3/build/Release/better_sqlite3.node"
    printf 'test ABI binding\n' \
        > "$package_root/node_modules/better-sqlite3/build/Release/better_sqlite3.abi${node_abi}.node"
    write_context_mode_test_manifest "$runtime_root"
}

prepare_context_mode_last_good() {
    local runtime_home="$1"
    local runtime_root
    runtime_root="$(context_mode_test_runtime_root "$runtime_home" 1.0.169)"
    local package_root="$runtime_root/node_modules/context-mode"
    local files_sha256
    prepare_context_mode_runtime "$runtime_home"
    files_sha256="$(test_sha256_file "$runtime_root/ags-files.sha256")"
    mkdir -p "$runtime_home/.local/state/ags"
    jq -n --arg root "$package_root" --arg files_sha256 "$files_sha256" \
      --arg platform "$FAKE_CONTEXT_RUNTIME_PLATFORM" \
      --arg arch "$FAKE_CONTEXT_RUNTIME_ARCH" \
      --argjson node_abi "$FAKE_CONTEXT_RUNTIME_NODE_ABI" \
      --arg target "$FAKE_CONTEXT_RUNTIME_TARGET" '{
      schema:2,
      managed_by:"ags",
      activation_id:"ags-test-last-good",
      version:"1.0.169",
      source:{
        type:"npm",
        package:"context-mode",
        integrity:"sha512-94JIaFuLjF9SO2BsGTrbGtyT44K95+9OC8BdbaL/UT76xOkanJLfUR5CzmNw+GELXZQqH4nBrKg9wjBnSFkVnQ==",
        files_sha256:$files_sha256
      },
      runtime:{
        platform:$platform,
        arch:$arch,
        node_abi:$node_abi,
        target:$target
      },
      package_root:$root,
      health:{mode:"offline-index-search",status:"passed"},
      providers:{}
    }' > "$runtime_home/.local/state/ags/context-mode.json"
    chmod 600 "$runtime_home/.local/state/ags/context-mode.json"
}

prepare_context_mode_last_good "$home"
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
        VERSION=v0.3.0-agsx.1 \
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
context_runtime="$(context_mode_test_runtime_root "$home" 1.0.169)"
context_root="$context_runtime/node_modules/context-mode"
jq -e '.name == "context-mode" and .version == "1.0.169"' \
    "$context_root/package.json" >/dev/null
for marker in claude-marketplace claude-plugin codex-marketplace codex-plugin \
    codex-hooks; do
    [[ -f "$home/.local/state/context-mode-fake/$marker" ]]
done
grep -Fq 'CLAUDE_CONTEXT=plugin marketplace add ' "$home/.context-mode-test.log"
grep -Fq 'CLAUDE_CONTEXT=plugin install context-mode@context-mode --scope user' \
    "$home/.context-mode-test.log"
grep -Fq 'CODEX_CONTEXT=plugin marketplace add ' "$home/.context-mode-test.log"
grep -Fq 'CODEX_CONTEXT=plugin add context-mode@context-mode --json' \
    "$home/.context-mode-test.log"
grep -Fqx 'CODEX_CONTEXT=features enable hooks' "$home/.context-mode-test.log"
if grep -Fq -- '--dangerously-bypass-hook-trust' \
    "$home/.context-mode-test.log" "$tmp/first.out" "$tmp/first.err"; then
    printf 'installer bypassed Codex hook trust\n' >&2
    exit 1
fi

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
prepare_context_mode_last_good "$rollback_home"
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
    VERSION=v0.3.0-agsx.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$rollback_bin" \
    --no-verify --quiet > "$tmp/rollback.out" 2> "$tmp/rollback.err"; then
    printf 'installer ignored a mandatory Context Mode health failure\n' >&2
    exit 1
fi
grep -Fq 'Context Mode initialization failed for codex' "$tmp/rollback.err"
[[ "$rollback_binary_sha" == "$(sha256sum "$rollback_bin/casr" | cut -d' ' -f1)" ]]
[[ "$("$rollback_bin/casr")" == preexisting-casr ]]
[[ ! -e "$rollback_bin/rmux" && ! -e "$rollback_bin/rmux-daemon" ]]
[[ ! -e "$rollback_bin/libexec/rmux/rmux" ]]
[[ -d "$(context_mode_test_runtime_root "$rollback_home" 1.0.169)" ]]
for marker in claude-marketplace claude-plugin codex-marketplace codex-plugin; do
    [[ ! -e "$rollback_state/$marker" ]]
done
[[ ! -e "$rollback_home/.codex/config.toml" ]]
[[ ! -e "$rollback_home/.local/state/ags/context-mode.pending.json" ]]
[[ ! -e "$rollback_home/.config/ags/identity.agekey" ]]
[[ ! -e "$rollback_home/.local/state/ags/storage.json" ]]
[[ "$rollback_context_sha" == "$(
    test_sha256_file "$rollback_home/.local/state/ags/context-mode.json"
)" ]]
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
prepare_context_mode_last_good "$interrupted_home"
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
    VERSION=v0.3.0-agsx.1 \
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
prepare_context_mode_last_good "$rmux_partial_home"
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
    VERSION=v0.3.0-agsx.1 \
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
prepare_context_mode_last_good "$rmux_resume_home"
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
    VERSION=v0.3.0-agsx.1 \
    "$project_root/install.sh" --offline "$rmux_resume_missing" \
    --dest "$rmux_resume_bin" --no-verify \
    > "$tmp/rmux-resume.out" 2> "$tmp/rmux-resume.err"
grep -Fq 'Recovering interrupted RMUX and Context Mode installation' \
    "$tmp/rmux-resume.out" "$tmp/rmux-resume.err"
grep -Fq 'Resuming the interrupted casr, RMUX, and Context Mode installation' \
    "$tmp/rmux-resume.out" "$tmp/rmux-resume.err"
grep -Fq 'Finishing the recovered casr, RMUX, and Context Mode transaction' \
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
prepare_context_mode_last_good "$new_failure_home"
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
    VERSION=v0.3.0-agsx.1 \
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
prepare_context_mode_last_good "$unmanaged_home"
printf 'unmanaged\n' > "$unmanaged_bin/ags"
env HOME="$unmanaged_home" \
    XDG_CONFIG_HOME="$unmanaged_home/.config" \
    XDG_DATA_HOME="$unmanaged_home/.local/share" \
    XDG_STATE_HOME="$unmanaged_home/.local/state" \
    PATH="$offline_guard_bin:$unmanaged_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-agsx.1 \
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
prepare_context_mode_last_good "$symlink_home"
printf '{"outside":true}\n' > "$hook_outside"
ln -s "$skill_outside" "$symlink_home/.codex/skills/ags"
ln -s "$hook_outside" "$symlink_home/.claude/settings.json"
env HOME="$symlink_home" \
    XDG_CONFIG_HOME="$symlink_home/.config" \
    XDG_DATA_HOME="$symlink_home/.local/share" \
    XDG_STATE_HOME="$symlink_home/.local/state" \
    PATH="$symlink_tools:$symlink_bin:/usr/bin:/bin" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-agsx.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$symlink_bin" \
    --no-verify > "$tmp/symlink.out" 2> "$tmp/symlink.err"
[[ ! -e "$skill_outside/SKILL.md" ]]
grep -Fqx '{"outside":true}' "$hook_outside"
grep -Fq 'Checkpoint skill path contains a symbolic link' "$tmp/symlink.out"
grep -Fq 'Checkpoint hook path contains a symbolic link' "$tmp/symlink.out"

missing_context_home="$tmp/missing-context-home"
missing_context_bin="$tmp/missing-context-bin"
mkdir -p "$missing_context_home" "$missing_context_bin"
if env HOME="$missing_context_home" \
    XDG_CONFIG_HOME="$missing_context_home/.config" \
    XDG_DATA_HOME="$missing_context_home/.local/share" \
    XDG_STATE_HOME="$missing_context_home/.local/state" \
    PATH="$offline_guard_bin:$missing_context_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION=v0.3.0-agsx.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$missing_context_bin" \
    --no-verify --quiet > "$tmp/missing-context.out" \
    2> "$tmp/missing-context.err"; then
    printf 'offline installer accepted a missing mandatory Context Mode runtime\n' >&2
    exit 1
fi
grep -Fq 'offline Context Mode initialization requires a validated last-good runtime; a fresh install must contact the official npm registry once' \
    "$tmp/missing-context.err"
[[ ! -e "$missing_context_bin/casr" ]]
[[ ! -e "$missing_context_bin/.casr.install-transaction.json" ]]
[[ -z "$(find "$missing_context_bin" -maxdepth 1 -type f \
    -name '.casr.rollback.*' -print -quit)" ]]

old_node_home="$tmp/old-node-home"
old_node_bin="$tmp/old-node-bin"
mkdir -p "$old_node_home" "$old_node_bin"
prepare_context_mode_last_good "$old_node_home"
if env HOME="$old_node_home" \
    XDG_CONFIG_HOME="$old_node_home/.config" \
    XDG_DATA_HOME="$old_node_home/.local/share" \
    XDG_STATE_HOME="$old_node_home/.local/state" \
    PATH="$offline_guard_bin:$old_node_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    FAKE_NODE_VERSION=v22.4.0 \
    VERSION=v0.3.0-agsx.1 \
    "$project_root/install.sh" --offline "$artifact" --dest "$old_node_bin" \
    --no-verify --quiet > "$tmp/old-node.out" 2> "$tmp/old-node.err"; then
    printf 'installer accepted Node older than Context Mode requires\n' >&2
    exit 1
fi
grep -Fq 'Context Mode requires Node.js 22.5.0 or newer' \
    "$tmp/old-node.err"
[[ ! -e "$old_node_bin/casr" ]]

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
ln -s "$FAKE_REAL_NODE_BINARY" "$online_tools/node-real"
prepare_context_mode_runtime "$online_fixture_home"
cat > "$online_tools/npm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "${FAKE_NPM_LOG:?}"
case "${1:-}" in
    view)
        jq -n '{
          version:"1.0.169",
          "dist.tarball":"https://registry.npmjs.org/context-mode/-/context-mode-1.0.169.tgz",
          "dist.integrity":"sha512-94JIaFuLjF9SO2BsGTrbGtyT44K95+9OC8BdbaL/UT76xOkanJLfUR5CzmNw+GELXZQqH4nBrKg9wjBnSFkVnQ==",
          license:"Elastic-2.0"
        }'
        ;;
    install)
        shift
        prefix=
        package=
        ignore_scripts=0
        while (( $# > 0 )); do
            case "$1" in
                --prefix)
                    prefix="$2"
                    shift 2
                    ;;
                --ignore-scripts)
                    ignore_scripts=1
                    shift
                    ;;
                --registry=*|--no-audit|--no-fund|--save-exact)
                    shift
                    ;;
                *)
                    package="$1"
                    shift
                    ;;
            esac
        done
        [[ "$prefix" == /* &&
           "$package" == context-mode@1.0.169 &&
           "$ignore_scripts" == 1 ]]
        mkdir -p "$prefix"
        cp -a "${FAKE_NPM_RUNTIME_SOURCE:?}/." "$prefix/"
        cp -a \
            "$prefix/node_modules/context-mode/node_modules/." \
            "$prefix/node_modules/"
        mkdir -p "$prefix/node_modules/.bin"
        ln -s ../context-mode/cli.bundle.mjs \
            "$prefix/node_modules/.bin/context-mode"
        printf '{"lockfileVersion":3}\n' \
            > "$prefix/node_modules/.package-lock.json"
        ;;
    *) exit 64 ;;
esac
EOF
chmod +x "$online_tools/npm"
cat > "$online_tools/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
destination=
while (( $# > 0 )); do
    case "$1" in
        -o)
            destination="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done
[[ -n "$destination" ]]
cp -- "${FAKE_ONLINE_ARTIFACT:?}" "$destination"
EOF
chmod +x "$online_tools/curl"
cat > "$online_tools/node" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == --version ]]; then
    printf 'v22.5.0\n'
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
[[ "$script" == */context-mode/cli.bundle.mjs ]]
case "${1:-}" in
    --help)
        printf 'context-mode online install fixture\n'
        ;;
    doctor)
        printf '%s\n' \
            'Storage session: PASS' \
            'Storage content: PASS' \
            'Storage stats: PASS' \
            'Server test: PASS' \
            'FTS5 / SQLite: PASS' \
            'Plugin enabled: PASS' \
            'PreToolUse hook: PASS' \
            'PostToolUse hook: PASS' \
            'SessionStart hook: PASS' \
            'PreCompact hook: PASS' \
            'UserPromptSubmit hook: PASS' \
            'Stop hook: PASS' \
            'Codex hooks feature flag: PASS' \
            'Codex plugin root: PASS'
        ;;
    *)
        exit 64
        ;;
esac
EOF
chmod +x "$online_tools/node"
cp -- "$offline_guard_bin/claude" "$online_tools/claude"
cp -- "$offline_guard_bin/codex" "$online_tools/codex"
chmod +x "$online_tools/claude" "$online_tools/codex"
env HOME="$online_home" \
    XDG_CONFIG_HOME="$online_home/.config" \
    XDG_DATA_HOME="$online_home/.local/share" \
    XDG_STATE_HOME="$online_home/.local/state" \
    PATH="$online_tools:$online_bin:$PATH" \
    FAKE_NPM_LOG="$online_npm_log" \
    FAKE_NPM_RUNTIME_SOURCE="$(
        context_mode_test_runtime_root "$online_fixture_home" 1.0.169
    )" \
    FAKE_ONLINE_ARTIFACT="$artifact" \
    ARTIFACT_URL=https://example.invalid/casr.tar.xz \
    VERSION=v0.3.0-agsx.1 \
    "$project_root/install.sh" --dest "$online_bin" --no-verify \
    --no-configure --no-skill --quiet \
    > "$tmp/online.out" 2> "$tmp/online.err"
grep -Fqx \
    'view --registry=https://registry.npmjs.org context-mode@latest version dist.tarball dist.integrity license --json' \
    "$online_npm_log"
grep -Eq \
    '^install --registry=https://registry\.npmjs\.org --prefix .*/\.context-mode-1\.0\.169\.install\.[^ ]+ --no-audit --no-fund --ignore-scripts --save-exact context-mode@1\.0\.169$' \
    "$online_npm_log"
online_context_runtime="$(
    context_mode_test_runtime_root "$online_home" 1.0.169
)"
[[ -x "$online_bin/casr" ]]
[[ -f "$online_context_runtime/ags-files.sha256" ]]
[[ -f "$online_context_runtime/node_modules/context-mode/server.bundle.mjs" ]]
jq -e '
    .health == {mode:"doctor",status:"passed"} and
    .source.package == "context-mode" and
    (.source.files_sha256 | type) == "string"
' "$online_home/.local/state/ags/context-mode.json" >/dev/null

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

printf 'agsx install smoke passed (%s/%s)\n' "$platform" "$(uname -m)"
