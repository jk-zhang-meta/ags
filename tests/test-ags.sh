#!/usr/bin/env bash
set -euo pipefail

tool="${1:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)/plugins/ags/scripts/ags}"
project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
test_platform="$(uname -s)"
test_tmp_root=/tmp
[[ "$test_platform" != Darwin ]] || test_tmp_root=/private/tmp
test_utf8_locale=
for candidate in C.UTF-8 C.utf8 en_US.UTF-8 UTF-8; do
    if [[ "$(LC_ALL="$candidate" locale charmap 2>/dev/null || true)" =~ ^UTF-?8$ ]]; then
        test_utf8_locale="$candidate"
        break
    fi
done
[[ -n "$test_utf8_locale" ]] || {
    echo 'tests require an installed UTF-8 locale' >&2
    exit 1
}
# macOS 的 /bin/bash 是 3.2.57，`"$dest（…"` 这种「变量紧跟全角字符」会被它把多字节
# 字符的首字节当成变量名的一部分，于是在 `set -u` 下报 `dest?: unbound variable` 而
# 整个脚本中止。Linux 上的 bash 5 完全正常，所以这个雷只在 Mac 上炸，而且只在真的
# 走到那一行时才炸——2026-08-08 就是这样在 `ags codext-update` 的最后一句 log 上炸的，
# 那时安装其实已经成功了，只是 stamp 没写成。
#
# 这条 lint 放在最前面，因为它必须在任何环境下都真的被执行到。修法永远是加花括号。
mapfile -t bare_var_before_multibyte < <(
    grep -nP '\$[A-Za-z_][A-Za-z0-9_]*[^\x00-\x7F]' "$tool" || true
)
(( ${#bare_var_before_multibyte[@]} == 0 )) || {
    printf 'bash 3.2 会把多字节字符的首字节吃进变量名，这些地方要写成 ${var}：\n' >&2
    printf '%s\n' "${bare_var_before_multibyte[@]}" >&2
    exit 1
}

tmp="$(mktemp -d "$test_tmp_root/agent-session-test.XXXXXX")"
declare -A test_child_pids=()

# 参数归属只看一个分隔符：`--` 之前整段归 ags（命令名前后都行），`--` 之后原样
# 交给 Agent。和 `cargo run --` / `kubectl exec pod --` 同一个约定，也和
# `ags resume ID [-- CLIENT_ARGS…]` 本来就有的约定一致——同一个 `--` 在这个工具里
# 只有一种含义。
#
# 放在文件最前面是因为它不需要 PTY、不需要编译产物，任何机器上都跑得到；而这个
# 套件是 `set -e`，靠后的用例在一台机器上红了，后面的就全不会执行。
ags_zone_home="$(mktemp -d "$tmp/ags-zone.XXXXXX")"
mkdir -p "$ags_zone_home/bin"
cat > "$ags_zone_home/bin/codext" <<'AGS_ZONE_FAKE'
#!/bin/sh
{ echo "ARGS=$*"
  echo "ACCOUNT=${CODEXT_POOL_ACCOUNT:-}"
  echo "DEVICE=${CODEXT_POOL_DEVICE_ID:-}"
} > "$AGS_ZONE_OUT"
AGS_ZONE_FAKE
cp "$ags_zone_home/bin/codext" "$ags_zone_home/bin/claude"
chmod +x "$ags_zone_home/bin/codext" "$ags_zone_home/bin/claude"

ags_zone_launch() {
    local out="$ags_zone_home/out"
    rm -f "$out"
    AGENT_SESSION_CODEX_BINARY="$ags_zone_home/bin/codext" \
        AGENT_SESSION_CLAUDE_BINARY="$ags_zone_home/bin/claude" \
        AGS_ZONE_OUT="$out" "$tool" "$@" >/dev/null 2>&1 || return $?
    cat "$out"
}

ags_zone_field() {
    sed -n "s/^$1=//p" <<< "$2"
}

ags_zone_expect() {
    local label="$1" field="$2" want="$3" got
    shift 3
    got="$(ags_zone_field "$field" "$(ags_zone_launch "$@")")" || {
        printf 'ags zone: %s: launch failed\n' "$label" >&2
        exit 1
    }
    [[ "$got" == "$want" ]] || {
        printf 'ags zone: %s: %s was %q, wanted %q\n' \
            "$label" "$field" "$got" "$want" >&2
        exit 1
    }
}

ags_zone_refuse() {
    local label="$1"
    shift
    if ags_zone_launch "$@" >/dev/null 2>&1; then
        printf 'ags zone: %s: expected a refusal\n' "$label" >&2
        exit 1
    fi
}

# `--` 之后原样交给 Agent，一个字都不解析。
ags_zone_expect 'agent args reach the agent' ARGS '--model o3' \
    codex -- --model o3
# 哪怕它和 ags 自己的参数同名：Agent 将来新增任何参数都不会被 ags 抢走，这是这个
# 约定最要紧的性质。
ags_zone_expect 'an ags-looking token after -- still belongs to the agent' \
    ARGS '--account not-ours@example.com' \
    codex -- --account not-ours@example.com
ags_zone_expect 'an ags-looking token after -- is not claimed by ags' \
    ACCOUNT '' \
    codex -- --account not-ours@example.com

# `--` 之前的一切归 ags，命令名前后都行。
ags_zone_expect 'an ags option before the command' ACCOUNT zone@example.com \
    --account zone@example.com codex
ags_zone_expect 'an ags option after the command' ACCOUNT zone@example.com \
    codex --account zone@example.com
ags_zone_expect 'both zones at once' ARGS '--model o3' \
    --account zone@example.com codex -- --model o3

# 光有账号不够：codext 的 device_id 默认按工作目录取，而服务端的租约表在
# device_id 上有唯一约束——同一个目录里两个点不同号的会话会撞同一条租约行。
ags_zone_first="$(ags_zone_field DEVICE \
    "$(ags_zone_launch --account zone@example.com codex)")"
[[ -n "$ags_zone_first" ]] || {
    printf 'ags zone: --account must also pin a lease identity\n' >&2
    exit 1
}
# 不同账号必须是不同的租约身份，同一个账号必须稳定——前者决定两个会话能不能并存，
# 后者决定反复启动会不会每次都留一个幽灵持有者去抬高服务端的并发除数。
[[ "$(ags_zone_field DEVICE "$(ags_zone_launch --account other@example.com codex)")" \
    != "$ags_zone_first" ]] || {
    printf 'ags zone: two accounts must not share one lease identity\n' >&2
    exit 1
}
ags_zone_expect 'the same account keeps one lease identity' DEVICE "$ags_zone_first" \
    --account zone@example.com codex
# 调用方显式给过的租约身份不能被覆盖。
[[ "$(CODEXT_POOL_DEVICE_ID=explicit-id ags_zone_field DEVICE \
    "$(CODEXT_POOL_DEVICE_ID=explicit-id ags_zone_launch --account zone@example.com codex)")" \
    == explicit-id ]] || {
    printf 'ags zone: an explicit CODEXT_POOL_DEVICE_ID must win\n' >&2
    exit 1
}

# 正对照：不带 --account 时 claude 必须起得来。没有这一条，下面"给 claude 必须被
# 拒"就可能只是因为测试环境里没有 claude 二进制——负对照抓到过一次。
ags_zone_launch claude >/dev/null || {
    printf 'ags zone: claude must launch when no pool option is given\n' >&2
    exit 1
}

# 拒绝路径。最后两条尤其要拒而不是警告：静默注入会让人以为号钉住了、其实什么都
# 没发生。倒数第三条是旧写法，报错里要指向 `--`。
ags_zone_refuse 'a missing --account value' --account
ags_zone_refuse 'both pool switches at once' --account a@b.com --pick-account codex
ags_zone_refuse 'a pool option on a non-agent command' --account a@b.com list
ags_zone_refuse 'an unknown ags option' --bogus codex
ags_zone_refuse 'an agent option without --' codex --model o3
ags_zone_refuse 'a pool option for claude' --account a@b.com claude
ags_zone_refuse 'the interactive picker for claude' --pick-account claude
ags_zone_refuse 'a resume-only option on a direct launch' codex --profile native

# 还原路径上的符号链接：解析之后按 StrictModes 判，不是一律拒绝。把两个
# `CODEX_HOME` 的 `sessions` 指到同一份是正当用法（共用一份会话历史），而一律
# 拒绝会让它完全不可用。放行的前提是解析后的目录归当前用户、组和其他人都不可写
# ——否则别人改一下那个目录就能把还原写到任意位置。
ags_link_dir="$(mktemp -d "$tmp/ags-link.XXXXXX")"
ags_link_fns="$(
    sed -n '/^path_owner_mode() {/,/^}/p;/^resolve_restore_component() {/,/^}/p' "$tool"
)"
grep -q '^resolve_restore_component() {' <<< "$ags_link_fns" || {
    printf 'ags link: could not extract resolve_restore_component from %s\n' "$tool" >&2
    exit 1
}
ags_link_probe() {
    bash -c '
        set -uo pipefail
        log() { printf "[ags] %s\n" "$*" >&2; }
        eval "$1"
        resolve_restore_component "$2"
    ' _ "$ags_link_fns" "$1" 2>/dev/null
}

mkdir -p "$ags_link_dir/real" "$ags_link_dir/loose"
chmod 755 "$ags_link_dir/real"
chmod 777 "$ags_link_dir/loose"
ln -sfn "$ags_link_dir/real" "$ags_link_dir/ok"
ln -sfn "$ags_link_dir/loose" "$ags_link_dir/world-writable"
ln -sfn "$ags_link_dir/nowhere" "$ags_link_dir/dangling"

[[ "$(ags_link_probe "$ags_link_dir/real")" == "$ags_link_dir/real" ]] || {
    printf 'ags link: a plain directory must pass through unchanged\n' >&2
    exit 1
}
[[ "$(ags_link_probe "$ags_link_dir/ok")" == "$ags_link_dir/real" ]] || {
    printf 'ags link: a link to an own, mode-clean directory must resolve\n' >&2
    exit 1
}
if ags_link_probe "$ags_link_dir/world-writable" >/dev/null 2>&1; then
    printf 'ags link: a link into a world-writable directory must be refused\n' >&2
    exit 1
fi
if ags_link_probe "$ags_link_dir/dangling" >/dev/null 2>&1; then
    printf 'ags link: a dangling link must be refused\n' >&2
    exit 1
fi
unset ags_link_fns ags_link_dir
export FAKE_REAL_NODE_BINARY="$(command -v node)"
export FAKE_REAL_RM_BINARY="$(command -v rm)"
export FAKE_REAL_MV_BINARY="$(command -v mv)"
[[ -x "$FAKE_REAL_NODE_BINARY" ]]
[[ -x "$FAKE_REAL_RM_BINARY" ]]
[[ -x "$FAKE_REAL_MV_BINARY" ]]

if [[ "$test_platform" == Darwin ]]; then
    brew_prefix="${HOMEBREW_PREFIX:-$(brew --prefix)}"
    export PATH="$brew_prefix/opt/coreutils/libexec/gnubin:$brew_prefix/opt/findutils/libexec/gnubin:$brew_prefix/opt/gnu-tar/libexec/gnubin:$brew_prefix/bin:$brew_prefix/opt/util-linux/bin:$PATH"
fi

cc "$project_root/tests/ags-agent-holder.c" -o "$tmp/codex"
cp -- "$tmp/codex" "$tmp/claude"

cleanup() {
    local pid
    for pid in "${!test_child_pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in "${!test_child_pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
    rm -rf -- "$tmp"
}

stop_test_process() {
    local pid="$1"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    unset "test_child_pids[$pid]"
}

test_process_start_time() {
    local pid="$1" stat
    local -a fields=()
    if [[ -r "/proc/$pid/stat" ]]; then
        IFS= read -r stat < "/proc/$pid/stat"
        read -ra fields <<< "${stat##*) }"
        printf '%s\n' "${fields[19]}"
    else
        LC_ALL=C TZ=UTC ps -o lstart= -p "$pid" |
            sed 's/^[[:space:]]*//;s/[[:space:]]*$//'
    fi
}

test_process_name() {
    local pid="$1" name
    if [[ -r "/proc/$pid/status" ]]; then
        sed -n 's/^Name:[[:space:]]*//p' "/proc/$pid/status"
    else
        name="$(ps -o comm= -p "$pid" 2>/dev/null || true)"
        name="${name#"${name%%[![:space:]]*}"}"
        name="${name%"${name##*[![:space:]]}"}"
        printf '%s\n' "${name##*/}"
    fi
}

test_process_running() {
    local pid="$1" process_status
    process_status="$(
        LC_ALL=C ps -o stat= -p "$pid" 2>/dev/null |
            sed 's/^[[:space:]]*//;s/[[:space:]]*$//'
    )"
    [[ -n "$process_status" && "$process_status" != Z* ]]
}

test_fd9_target() {
    local pid="$1"
    if [[ -e "/proc/$pid/fd/9" ]]; then
        readlink -- "/proc/$pid/fd/9"
    else
        lsof -a -p "$pid" -d 9 -Fn 2>/dev/null |
            sed -n 's/^n//p' | head -n 1
    fi
}

test_process_has_fd_target() {
    local pid="$1" expected="$2" fd target
    if [[ -d "/proc/$pid/fd" ]]; then
        for fd in "/proc/$pid/fd"/*; do
            target="$(readlink -- "$fd" 2>/dev/null || true)"
            [[ "$target" != "$expected" ]] || return 0
        done
        return 1
    fi
    lsof -a -p "$pid" -Fn 2>/dev/null |
        sed -n 's/^n//p' |
        grep -Fqx "$expected"
}

start_agent_process() {
    local agent="$1" open_file="${2:-}" iteration
    if [[ -n "$open_file" ]]; then
        "$tmp/$agent" "$open_file" &
    else
        "$tmp/$agent" &
    fi
    active_test_pid=$!
    test_child_pids["$active_test_pid"]=1
    for iteration in {1..100}; do
        if [[ "$(test_process_name "$active_test_pid")" == "$agent" ]] &&
           { [[ -z "$open_file" ]] ||
             [[ "$(test_fd9_target "$active_test_pid")" == \
                "$open_file" ]]; }; then
            return
        fi
        sleep 0.01
    done
    printf 'failed to start the %s test process\n' "$agent" >&2
    return 1
}

start_codex_session_process() {
    local native_home="$1" session_id="$2"
    local relative="${3:-sessions/2026/07/25/rollout-active-$session_id.jsonl}"
    active_codex_path="$native_home/$relative"
    mkdir -p "${active_codex_path%/*}"
    if [[ ! -e "$active_codex_path" ]]; then
        printf '{"type":"session_meta","payload":{"id":"%s"}}\n' "$session_id" \
            > "$active_codex_path"
    fi
    start_agent_process codex "$active_codex_path"
}

start_claude_session_process() {
    local native_home="$1" session_id="$2" stale_start="${3:-0}"
    local start_time
    start_agent_process claude
    start_time="$(test_process_start_time "$active_test_pid")"
    if (( stale_start == 1 )); then
        if [[ "$start_time" =~ ^[0-9]+$ ]]; then
            start_time=$((start_time + 1))
        else
            start_time="$start_time stale"
        fi
    fi
    mkdir -p "$native_home/sessions"
    active_claude_registry="$native_home/sessions/$active_test_pid.json"
    jq -n --argjson pid "$active_test_pid" --arg session_id "$session_id" \
        --arg proc_start "$start_time" \
        '{pid: $pid, sessionId: $session_id, procStart: $proc_start,
          status: "active", version: "test"}' > "$active_claude_registry"
    jq -e '(.procStart | type) == "string"' "$active_claude_registry" >/dev/null
    chmod 600 "$active_claude_registry"
}

write_codex_profile() {
    local native_home="$1" profile="$2" provider="${3:-openai}"
    mkdir -p "$native_home"
    printf 'model_provider = "%s"\n' "$provider" \
        > "$native_home/$profile.config.toml"
}

trap cleanup EXIT

test_sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$1" | awk '{print $1}'
    else
        shasum -a 256 -- "$1" | awk '{print $1}'
    fi
}

ssh-keygen -q -t ed25519 -N '' -f "$tmp/key"
mkdir -p "$tmp/home/.local/bin"
printf '%s\n' '#!/usr/bin/env sh' \
    'printf '\''%s\n'\'' '\''{"success":true,"ip":"203.0.113.7","country":"Test Country","region":"Test Region","city":"Test City","latitude":1.25,"longitude":2.5}'\''' \
    > "$tmp/home/.local/bin/curl"
chmod +x "$tmp/home/.local/bin/curl"
cat > "$tmp/home/.local/bin/node" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == --version ]]; then
    printf 'v22.5.0\n'
    exit
fi
script="${1:-}"
shift || true
if [[ "$script" == - ]]; then
    exec "${FAKE_REAL_NODE_BINARY:?}" - "$@"
fi
exec "${FAKE_REAL_NODE_BINARY:?}" "$script" "$@"
EOF
chmod +x "$tmp/home/.local/bin/node"
cat > "$tmp/home/.local/bin/codex" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
log="${BASH_SOURCE[0]%/*}/../ags.log"
if [[ "${1:-}" == --version ]]; then
    printf 'codex-test 1.0\n'
    exit
fi
profile_before_subcommand=0
case "${1:-}" in
    --profile|-p)
        [[ -n "${2:-}" ]] || exit 64
        profile_before_subcommand=1
        shift 2
        ;;
    --profile=*|-p?*)
        profile_before_subcommand=1
        shift
        ;;
esac
if (( profile_before_subcommand == 1 )) && [[ "${1:-}" == app-server ]]; then
    printf '%s\n' \
        'Error: --profile only applies to runtime commands and `codex mcp`.' >&2
    exit 2
fi
case "${1:-}" in
    resume) [[ ! -e "/dev/fd/9" ]] || { printf 'FD9_OPEN\n'; exit 99; } ;;
esac
printf 'LAUNCH=%s %s\n' "$0" "$*" >> "$log"
printf 'FAKE_CODEX'
for argument in "$@"; do printf ' <%s>' "$argument"; done
printf '\nFAKE_PWD=%s\n' "$PWD"
printf 'FAKE_AGS_LAUNCH_ARGS=%s\n' "${AGS_LAUNCH_ARGS-unset}"
EOF
chmod +x "$tmp/home/.local/bin/codex"
cat > "$tmp/home/.local/bin/claude" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
log="${BASH_SOURCE[0]%/*}/../ags.log"
if [[ -n "${AGENT_SESSION_REMOTE_PASSWORD:-}" ||
      -n "${AGENT_SESSION_CLOUD_PASSWORD:-}" ||
      -n "${RCLONE_SFTP_PASS:-}" ]]; then
    printf 'CLAUDE_TRANSPORT_SECRET_LEAK\n' >&2
    exit 98
fi
if [[ "${1:-}" == --version ]]; then
    printf 'claude-test 1.0\n'
    exit
fi
case "${1:-}" in
    --resume) [[ ! -e "/dev/fd/9" ]] || { printf 'FD9_OPEN\n'; exit 99; } ;;
esac
printf 'LAUNCH=%s %s\n' "$0" "$*" >> "$log"
printf 'FAKE_CLAUDE'
for argument in "$@"; do printf ' <%s>' "$argument"; done
printf '\nFAKE_PWD=%s\n' "$PWD"
printf 'FAKE_AGS_LAUNCH_ARGS=%s\n' "${AGS_LAUNCH_ARGS-unset}"
EOF
chmod +x "$tmp/home/.local/bin/claude"
cat > "$tmp/home/.local/bin/casr" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

log="${BASH_SOURCE[0]%/*}/../ags.log"
if [[ "${1:-}" == --version ]]; then
    printf 'casr 0.3.0-test\n'
    exit
fi
[[ ! -e "/dev/fd/9" ]]

if [[ "${1:-}" == checkpoint-register-codex ]]; then
    [[ $# == 4 && -f "$3" && "$4" == /* ]]
    printf 'REGISTER=%s\t%s\t%s\n' "$2" "$3" "$4" >> "$log"
    [[ "${AGS_REGISTER_FAIL:-0}" == 0 ]]
    exit
fi


[[ "${1:-}" == --json ]]
shift
[[ -z "${OPENAI_API_KEY:-}" && -z "${ANTHROPIC_API_KEY:-}" ]]
case "${1:-}" in
    resume)
        target="${2:-}"
        source_id="${3:-}"
        shift 3
        source=
        force=0
        no_store=0
        while (( $# )); do
            case "$1" in
                --source) source="$2"; shift 2 ;;
                --force) force=1; shift ;;
                --no-store) no_store=1; shift ;;
                *) exit 64 ;;
            esac
        done
        [[ -f "$source" && "$force" == 1 && "$no_store" == 1 ]]
        case "$target" in
            cc)
                source_format=codex
                target_format=claude-code
                id=bbbbbbbb-cccc-4ddd-8eee-ffffffffffff
                path="$CLAUDE_CONFIG_DIR/projects/-untrusted-source-path/$id.jsonl"
                mkdir -p "${path%/*}"
                printf '%s\n' \
                    '{"parentUuid":null,"isSidechain":false,"cwd":"/source","type":"user","message":{"role":"user","content":[{"type":"text","text":"converted user"}]},"uuid":"11111111-1111-4111-8111-111111111111","sessionId":"bbbbbbbb-cccc-4ddd-8eee-ffffffffffff"}' \
                    '{"parentUuid":"11111111-1111-4111-8111-111111111111","isSidechain":false,"cwd":"/source","type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"converted reasoning summary"}]},"uuid":"22222222-2222-4222-8222-222222222222","sessionId":"bbbbbbbb-cccc-4ddd-8eee-ffffffffffff"}' \
                    > "$path"
                ;;
            cod)
                source_format=claude-code
                target_format=codex
                id=01999999-aaaa-7bbb-8ccc-dddddddddddd
                path="$CODEX_HOME/sessions/2026/07/25/rollout-test-$id.jsonl"
                mkdir -p "${path%/*}"
                printf '%s\n' \
                    '{"timestamp":"2026-07-25T00:00:00Z","type":"session_meta","payload":{"id":"01999999-aaaa-7bbb-8ccc-dddddddddddd","cwd":"/source","model_provider":"openai"}}' \
                    '{"timestamp":"2026-07-25T00:00:01Z","type":"turn_context","payload":{"workspace_roots":["/source"]}}' \
                    '{"timestamp":"2026-07-25T00:00:02Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"converted user"}]}}' \
                    > "$path"
                ;;
            *) exit 64 ;;
        esac
        printf 'CONVERT=%s\t%s\t%s\n' "$source_format" "$target_format" "$source" >> "$log"
        printf 'HOME=%s\nCODEX_HOME=%s\nCLAUDE_CONFIG_DIR=%s\nNO_STORE=%s\n' \
            "$HOME" "$CODEX_HOME" "$CLAUDE_CONFIG_DIR" "$no_store" >> "$log"
        jq -n --arg source "$source_format" --arg target "$target_format" \
            --arg source_id "$source_id" --arg id "$id" --arg path "$path" '{
              ok: true, source_provider: $source, target_provider: $target,
              source_session_id: $source_id, target_session_id: $id,
              written_paths: [$path], resume_command: "unused", dry_run: false,
              fidelity: "conversation_only", verified_fidelity: null,
              losses: [{kind:"reasoning", note:"provider-bound reasoning was summarized"}],
              warnings: ["test conversion warning"]
            }'
        ;;
    info)
        path="${2:-}"
        shift 2
        [[ "${1:-}" == --from && -f "$path" ]]
        format="$2"
        case "$format" in
            cc)
                detected=claude-code
                id="$(jq -sr '.[0].sessionId' "$path")"
                ;;
            cod)
                detected=codex
                id="$(jq -sr 'map(select(.type == "session_meta"))[0].payload.id' "$path")"
                ;;
            *) exit 64 ;;
        esac
        jq -n --arg id "$id" --arg detected "$detected" \
            '{session_id:$id, detected_format:$detected}'
        ;;
    *) exit 64 ;;
esac
EOF
chmod +x "$tmp/home/.local/bin/casr"
cat > "$tmp/home/.local/bin/rm" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ -n "${FAKE_RM_PENDING_HOLD_DIR:-}" ]]; then
    for argument in "$@"; do
        case "$argument" in
            */pending-sync/*.json)
                mkdir -p "$FAKE_RM_PENDING_HOLD_DIR"
                : > "$FAKE_RM_PENDING_HOLD_DIR/ready"
                released=0
                for _ in {1..1000}; do
                    if [[ -e "$FAKE_RM_PENDING_HOLD_DIR/release" ]]; then
                        released=1
                        break
                    fi
                    sleep 0.01
                done
                (( released == 1 )) || {
                    printf 'fake pending removal hold timed out\n' >&2
                    exit 3
                }
                break
                ;;
        esac
    done
fi
exec "$FAKE_REAL_RM_BINARY" "$@"
EOF
chmod +x "$tmp/home/.local/bin/rm"
cat > "$tmp/home/.local/bin/mv" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
source_path=
destination_path=
for argument in "$@"; do
    [[ "$argument" == -- ]] && continue
    if [[ -z "$source_path" ]]; then
        source_path="$argument"
    else
        destination_path="$argument"
    fi
done
if [[ -n "${FAKE_MV_DELETE_ARCHIVE_ONCE:-}" &&
      "$source_path" == *.checkpoint.tar.gz.age &&
      "$destination_path" == */trash/local/*/*.checkpoint.tar.gz.age.deleted.* &&
      ! -e "$FAKE_MV_DELETE_ARCHIVE_ONCE" ]]; then
    : > "$FAKE_MV_DELETE_ARCHIVE_ONCE"
    exit 1
fi
if [[ -n "${FAKE_MV_PENDING_SYNC_ONCE:-}" &&
      "$source_path" == */pending-sync/*.json.tmp.* &&
      "$destination_path" == */pending-sync/*.json &&
      ! -e "$FAKE_MV_PENDING_SYNC_ONCE" ]]; then
    : > "$FAKE_MV_PENDING_SYNC_ONCE"
    exit 1
fi
exec "$FAKE_REAL_MV_BINARY" "$@"
EOF
chmod +x "$tmp/home/.local/bin/mv"
printf '%s\n' '#!/usr/bin/env sh' \
    'case " $* " in *" -dc "*) eval "last=\${$#}"; cat -- "$last";; *) exit 0;; esac' \
    > "$tmp/home/.local/bin/zstd"
chmod +x "$tmp/home/.local/bin/zstd"
if [[ "$test_platform" == Darwin ]]; then
    printf '%s\n' '#!/usr/bin/env sh' 'exec /usr/bin/pgrep "$@"' \
        > "$tmp/home/.local/bin/pgrep"
else
    printf '%s\n' '#!/usr/bin/env sh' 'exit 1' > "$tmp/home/.local/bin/pgrep"
fi
chmod +x "$tmp/home/.local/bin/pgrep"
mkdir -p "$tmp/home/.ssh" "$tmp/remote"
read -r host_key_type host_key_data _ < "$tmp/key.pub"
printf '[127.0.0.1]:2222 %s %s\n' "$host_key_type" "$host_key_data" \
    > "$tmp/home/.ssh/known_hosts"
cat > "$tmp/home/.local/bin/rclone" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

map_path() {
    case "$1" in
        :sftp,*:/*) printf '%s%s\n' "$FAKE_RCLONE_ROOT" "${1##*:}" ;;
        *) printf '%s\n' "$1" ;;
    esac
}

while (( $# > 0 )); do
    case "$1" in
        --config|--log-level)
            (( $# >= 2 )) || exit 64
            shift 2
            ;;
        --ask-password=false)
            shift
            ;;
        *)
            break
            ;;
    esac
done
command="$1"
shift
if [[ -n "${FAKE_RCLONE_LOG:-}" ]]; then
    printf '%s' "$command" >> "$FAKE_RCLONE_LOG"
    for argument in "$@"; do printf '\t%s' "$argument" >> "$FAKE_RCLONE_LOG"; done
    if [[ -n "${RCLONE_SFTP_PASS:-}" ]]; then
        printf '\tAUTH_ENV=1' >> "$FAKE_RCLONE_LOG"
    fi
    printf '\n' >> "$FAKE_RCLONE_LOG"
fi
case "$command" in
    obscure)
        if [[ "${1:-}" == - ]]; then
            IFS= read -r password
        else
            password="${1:-}"
        fi
        printf 'obscured:%s\n' "$(printf '%s' "$password" | sha256sum | cut -c1-16)"
        unset password
        ;;
    copyto)
        if [[ -n "${FAKE_RCLONE_FAIL_RETIRE_READBACK_ONCE:-}" &&
              "$1" == */.ags-retired/ags-v1.retired &&
              ! -e "$FAKE_RCLONE_FAIL_RETIRE_READBACK_ONCE" ]]; then
            : > "$FAKE_RCLONE_FAIL_RETIRE_READBACK_ONCE"
            exit 1
        fi
        source="$(map_path "$1")"
        destination="$(map_path "$2")"
        mkdir -p "$(dirname "$destination")"
        cp -- "$source" "$destination"
        ;;
    mkdir) mkdir -p "$(map_path "$1")" ;;
    deletefile)
        target="$(map_path "$1")"
        if [[ -n "${FAKE_RCLONE_FAIL_ARCHIVE_DELETE_ONCE:-}" &&
              "$target" == *.checkpoint.tar.gz.age &&
              ! -e "$FAKE_RCLONE_FAIL_ARCHIVE_DELETE_ONCE" ]]; then
            mkdir -p "$(dirname "$FAKE_RCLONE_FAIL_ARCHIVE_DELETE_ONCE")"
            : > "$FAKE_RCLONE_FAIL_ARCHIVE_DELETE_ONCE"
            exit 1
        fi
        rm -f -- "$target"
        ;;
    moveto)
        source="$(map_path "$1")"
        destination="$(map_path "$2")"
        mkdir -p "$(dirname "$destination")"
        if [[ "$destination" == */records/*/*.record ]]; then
            object="$(cut -f6 "$source")"
            store_root="${destination%%/records/*}"
            [[ -f "$store_root/$object" ]] || {
                printf 'record marker published before object: %s\n' "$destination" >&2
                exit 4
            }
        fi
        manifest_failure_marker=
        if [[ -n "${FAKE_RCLONE_FAIL_MANIFEST_MOVE_ONCE:-}" &&
              "$destination" == *.manifest.age ]]; then
            manifest_failure_marker="$FAKE_RCLONE_FAIL_MANIFEST_MOVE_ONCE"
            [[ "$manifest_failure_marker" != 1 ]] ||
                manifest_failure_marker="$FAKE_RCLONE_ROOT/.manifest-move-failed"
            if [[ ! -e "$manifest_failure_marker" ]]; then
                mkdir -p "$(dirname "$manifest_failure_marker")"
                : > "$manifest_failure_marker"
                exit 1
            fi
        fi
        if [[ -n "${FAKE_RCLONE_FAIL_LEGACY_TRASH_MANIFEST_ONCE:-}" &&
              "$source" == */codex/*.manifest.age &&
              "$destination" == */trash/codex/*.manifest.age.deleted.* &&
              ! -e "$FAKE_RCLONE_FAIL_LEGACY_TRASH_MANIFEST_ONCE" ]]; then
            : > "$FAKE_RCLONE_FAIL_LEGACY_TRASH_MANIFEST_ONCE"
            exit 1
        fi
        if [[ -n "${FAKE_RCLONE_FAIL_LEGACY_TRASH_ARCHIVE_ONCE:-}" &&
              "$source" == */codex/*.checkpoint.tar.gz.age &&
              "$destination" == */trash/codex/*.checkpoint.tar.gz.age.deleted.* &&
              ! -e "$FAKE_RCLONE_FAIL_LEGACY_TRASH_ARCHIVE_ONCE" ]]; then
            : > "$FAKE_RCLONE_FAIL_LEGACY_TRASH_ARCHIVE_ONCE"
            exit 1
        fi
        manifest_post_move_failure=
        if [[ -n "${FAKE_RCLONE_FAIL_MANIFEST_AFTER_MOVE_ONCE:-}" &&
              "$destination" == *.manifest.age &&
              ! -e "$FAKE_RCLONE_FAIL_MANIFEST_AFTER_MOVE_ONCE" ]]; then
            manifest_post_move_failure="$FAKE_RCLONE_FAIL_MANIFEST_AFTER_MOVE_ONCE"
        fi
        if [[ "${3:-}" == --immutable ]]; then
            ln -- "$source" "$destination" 2>/dev/null || exit 1
            rm -f -- "$source"
        else
            mv -- "$source" "$destination"
        fi
        if [[ -n "$manifest_post_move_failure" ]]; then
            mkdir -p "$(dirname "$manifest_post_move_failure")"
            : > "$manifest_post_move_failure"
            exit 1
        fi
        ;;
    lsf)
        remote="${*: -1}"
        root="$(map_path "$remote")"
        if [[ -n "${FAKE_RCLONE_FAIL_LSF_AT:-}" ]]; then
            counter_file="${FAKE_RCLONE_LSF_COUNTER_FILE:?}"
            counter=0
            [[ ! -f "$counter_file" ]] || counter="$(<"$counter_file")"
            counter=$((counter + 1))
            printf '%s\n' "$counter" > "$counter_file"
            if (( counter == FAKE_RCLONE_FAIL_LSF_AT )); then
                exit 1
            fi
        fi
        if [[ -n "${FAKE_RCLONE_LSF_HOLD_DIR:-}" ]]; then
            mkdir -p "$FAKE_RCLONE_LSF_HOLD_DIR"
            : > "$FAKE_RCLONE_LSF_HOLD_DIR/ready"
            released=0
            for _ in {1..1000}; do
                if [[ -e "$FAKE_RCLONE_LSF_HOLD_DIR/release" ]]; then
                    released=1
                    break
                fi
                sleep 0.01
            done
            (( released == 1 )) || {
                printf 'fake rclone hold timed out\n' >&2
                exit 3
            }
        fi
        if [[ -n "${FAKE_RCLONE_LSF_BARRIER:-}" ]]; then
            mkdir -p "$FAKE_RCLONE_LSF_BARRIER"
            touch "$FAKE_RCLONE_LSF_BARRIER/ready.$$"
            ready=0
            for _ in {1..500}; do
                ready="$(find "$FAKE_RCLONE_LSF_BARRIER" -type f -name 'ready.*' | wc -l)"
                (( ready >= 2 )) && break
                sleep 0.01
            done
            (( ready >= 2 )) || {
                printf 'fake rclone barrier timed out\n' >&2
                exit 3
            }
        fi
        [[ -d "$root" ]] && find "$root" -type f -printf '%P\n' | sort
        ;;
    *) printf 'unsupported fake rclone command: %s\n' "$command" >&2; exit 2 ;;
esac
EOF
chmod +x "$tmp/home/.local/bin/rclone"
cat > "$tmp/home/.local/bin/ssh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

remote_command="${*: -1}"
IFS= read -r remote_root || {
    printf 'fake ssh expected a remote root on stdin\n' >&2
    exit 2
}
mapfile -t remote_input
if [[ -n "${FAKE_SSH_LOG:-}" ]]; then
    printf 'ssh' >> "$FAKE_SSH_LOG"
    for argument in "$@"; do
        printf '\t%s' "$argument" >> "$FAKE_SSH_LOG"
    done
    [[ -z "${AGENT_SESSION_REMOTE_PASSWORD:-}" ]] ||
        printf '\tPLAINTEXT_ENV_LEAK=1' >> "$FAKE_SSH_LOG"
    [[ -z "${AGENT_SESSION_CLOUD_PASSWORD:-}" ]] ||
        printf '\tCLOUD_PASSWORD_ENV_LEAK=1' >> "$FAKE_SSH_LOG"
    [[ -z "${RCLONE_SFTP_PASS:-}" ]] ||
        printf '\tRCLONE_PASSWORD_ENV_LEAK=1' >> "$FAKE_SSH_LOG"
    printf '\tSTDIN=%s' "$remote_root" >> "$FAKE_SSH_LOG"
    for input in "${remote_input[@]}"; do
        printf '\tSTDIN=%s' "$input" >> "$FAKE_SSH_LOG"
    done
    printf '\n' >> "$FAKE_SSH_LOG"
fi
if [[ -n "${FAKE_SSH_FAIL_RETIRE_ONCE_MARKER:-}" &&
      "$remote_command" == *'pending="$retired_root/ags-v1.retiring"'* &&
      ! -e "$FAKE_SSH_FAIL_RETIRE_ONCE_MARKER" ]]; then
    : > "$FAKE_SSH_FAIL_RETIRE_ONCE_MARKER"
    exit 70
fi
[[ "$remote_root" == /* && "$remote_root" != *$'\n'* &&
   "$remote_root" != *$'\r'* ]] || exit 64
{
    printf '%s%s\n' "${FAKE_RCLONE_ROOT:?}" "$remote_root"
    for input in "${remote_input[@]}"; do
        printf '%s\n' "$input"
    done
} | /usr/bin/env bash -c "$remote_command"
EOF
chmod +x "$tmp/home/.local/bin/ssh"
cat > "$tmp/home/.local/bin/sshpass" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

[[ "${1:-}" == -d && "${2:-}" =~ ^[0-9]+$ ]] || exit 64
password_fd="$2"
shift 2
IFS= read -r password <&"$password_fd" || true
if [[ -n "${FAKE_SSH_LOG:-}" ]]; then
    printf 'sshpass\tPASSWORD_FD=1\tPASSWORD_SHA256=%s\n' \
        "$(printf '%s' "$password" | sha256sum | cut -d' ' -f1)" \
        >> "$FAKE_SSH_LOG"
fi
unset password
exec "$@"
EOF
chmod +x "$tmp/home/.local/bin/sshpass"
mkdir -p "$tmp/source/codex/sessions/2026/01/01" "$tmp/source/claude/projects/-work-demo"
printf '%s\n' '{"turn":1}' > "$tmp/source/codex/sessions/2026/01/01/test.jsonl"
printf '%s\n' '{"turn":1}' > "$tmp/source/claude/projects/-work-demo/test.jsonl"

source_env=(
    HOME="$tmp/home"
    PATH="$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin"
    CODEX_HOME="$tmp/source/codex"
    CLAUDE_CONFIG_DIR="$tmp/source/claude"
    CODEX_THREAD_ID=
    CLAUDE_CODE_SESSION_ID=
    AGENT_SESSION_DIR="$tmp/store"
    AGENT_SESSION_STATE_DIR="$tmp/state"
    AGENT_SESSION_SSH_KEY="$tmp/key"
    AGS_CONVERTER_BINARY="$tmp/home/.local/bin/casr"
    AGS_CONVERTER_VERSION=0.3.0-test
    FAKE_RCLONE_ROOT="$tmp/remote"
)

printf '{malformed hook input\n' | \
    env "${source_env[@]}" "$tool" hook \
    >"$tmp/malformed-hook.out" 2>"$tmp/malformed-hook.err"
grep -Fq 'warning: hook processing failed; pending work was left for retry' \
    "$tmp/malformed-hook.err"

mkdir -p "$tmp/hook-lock-failure-bin" "$tmp/state/pending-sync"
printf '%s\n' '#!/bin/sh' 'exit 1' > "$tmp/hook-lock-failure-bin/flock"
chmod +x "$tmp/hook-lock-failure-bin/flock"
jq -n '{
  schema:1,
  storage_mode:"remote:neburst",
  reason:"hook lock failure regression",
  created_utc:"2026-07-31T00:00:00.000Z"
}' > "$tmp/state/pending-sync/hook-lock-failure.json"
printf '%s\n' '{"hook_event_name":"SessionStart"}' | \
    env "${source_env[@]}" \
    PATH="$tmp/hook-lock-failure-bin:$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
    "$tool" hook >"$tmp/hook-lock-failure.out" \
    2>"$tmp/hook-lock-failure.err"
grep -Fq 'cannot acquire storage consolidation lock' \
    "$tmp/hook-lock-failure.err"
grep -Fq 'warning: hook processing failed; pending work was left for retry' \
    "$tmp/hook-lock-failure.err"
[[ -f "$tmp/state/pending-sync/hook-lock-failure.json" ]]
if env "${source_env[@]}" \
    PATH="$tmp/hook-lock-failure-bin:$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
    "$tool" flush >"$tmp/flush-lock-failure.out" \
    2>"$tmp/flush-lock-failure.err"; then
    echo 'direct flush ignored a storage lock failure' >&2
    exit 1
fi
grep -Fq 'cannot acquire storage consolidation lock' \
    "$tmp/flush-lock-failure.err"
rm -f -- "$tmp/state/pending-sync/hook-lock-failure.json"

target_env=(
    HOME="$tmp/home"
    PATH="$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin"
    CODEX_HOME="$tmp/target/codex"
    CLAUDE_CONFIG_DIR="$tmp/target/claude"
    CODEX_THREAD_ID=
    CLAUDE_CODE_SESSION_ID=
    AGENT_SESSION_DIR="$tmp/store"
    AGENT_SESSION_STATE_DIR="$tmp/state"
    AGENT_SESSION_SSH_KEY="$tmp/key"
    FAKE_RCLONE_ROOT="$tmp/remote"
)
mkdir -p "$tmp/managed-local-checkpoints"
managed_env=(
    "${source_env[@]}"
    AGENT_SESSION_LOCAL_DIR="$tmp/managed-local-checkpoints"
)
mkdir -p "$tmp/state"
converted_claude_id=bbbbbbbb-cccc-4ddd-8eee-ffffffffffff
converted_codex_id=01999999-aaaa-7bbb-8ccc-dddddddddddd


fresh_codex="$(
    env "${managed_env[@]}" "$tool" codex -- --model o3 \
        2> "$tmp/fresh-codex.err"
)"
grep -Fqx 'FAKE_CODEX <--model> <o3>' <<< "$fresh_codex"
# The launch hands its own arguments to the session it starts, because `ags save`
# runs as a child of the Agent and the environment is the only channel that
# reaches it — the argv itself is consumed by exec.
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","o3"]' <<< "$fresh_codex"
[[ ! -s "$tmp/fresh-codex.err" ]]
# An argument carrying a line break cannot round-trip the line-oriented
# manifest, so the launch records none of them rather than a truncated command
# line. The Agent still receives it unchanged.
newline_codex="$(env "${managed_env[@]}" "$tool" codex -- --model $'a\nb')"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=[]' <<< "$newline_codex"
# Starting a session by resuming one is an ordinary thing to do, and the Agent
# still receives that command line whole. The record does not: `ags resume`
# builds `codex resume <restored>` itself, so a replayed `resume` would name a
# second session after the one just restored, and Codex takes the later name.
resumed_codex="$(env "${managed_env[@]}" "$tool" codex -- resume --model o3)"
grep -Fqx 'FAKE_CODEX <resume> <--model> <o3>' <<< "$resumed_codex"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","o3"]' <<< "$resumed_codex"
# The session id is positional, so it leaves with the subcommand; the picker
# flags leave because `--last` outranks the name AGS supplies.
resumed_codex_id="$(
    env "${managed_env[@]}" "$tool" codex -- resume \
        99999999-8888-4777-8666-555555555555 --all --model o3
)"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","o3"]' <<< "$resumed_codex_id"
resumed_codex_last="$(env "${managed_env[@]}" "$tool" codex -- resume --last)"
grep -Fqx 'FAKE_CODEX <resume> <--last>' <<< "$resumed_codex_last"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=[]' <<< "$resumed_codex_last"
mkdir -p "$tmp/source/codex"
printf '%s\n' 'model_provider = "sub2api"' \
    > "$tmp/source/codex/sub2api.config.toml"
assert_fresh_codex_profile() {
    local expected="$1" output
    shift
    output="$(env "${managed_env[@]}" "$tool" codex -- "$@")"
    grep -Fqx "$expected" <<< "$output"
}
assert_fresh_codex_profile \
    'FAKE_CODEX <--yolo> <--profile> <sub2api>' \
    --yolo --profile sub2api
assert_fresh_codex_profile \
    'FAKE_CODEX <--yolo> <--profile=sub2api>' \
    --yolo --profile=sub2api
assert_fresh_codex_profile \
    'FAKE_CODEX <--yolo> <-p> <sub2api>' \
    --yolo -p sub2api
assert_fresh_codex_profile \
    'FAKE_CODEX <--yolo> <-psub2api>' \
    --yolo -psub2api
agent_owned_description="$(
    env "${managed_env[@]}" "$tool" codex -- --description native-description
)"
grep -Fqx 'FAKE_CODEX <--description> <native-description>' \
    <<< "$agent_owned_description"
if grep -Eq '^CODEX_APP_SERVER=.*(^|[[:space:]])(-p|--profile)' \
    "$tmp/home/.local/ags.log"; then
    echo 'managed Codex leaked a runtime profile into app-server' >&2
    exit 1
fi
scrubbed_codex="$(
    env "${managed_env[@]}" \
        AGENT_SESSION_REMOTE_PASSWORD=remote-secret \
        AGENT_SESSION_CLOUD_PASSWORD=cloud-secret \
        RCLONE_SFTP_PASS=rclone-secret \
        "$tool" codex -- --model scrubbed
)"
grep -Fqx 'FAKE_CODEX <--model> <scrubbed>' <<< "$scrubbed_codex"
grep -Eq '^LAUNCH=.*/codex --model o3$' \
    "$tmp/home/.local/ags.log"
fresh_codex_owned_args="$(
    env "${managed_env[@]}" "$tool" codex -- --to claude --model o3
)"
grep -Fqx \
    'FAKE_CODEX <--to> <claude> <--model> <o3>' \
    <<< "$fresh_codex_owned_args"
# `--` 之后原样交给 Agent，哪怕那个名字和 ags 自己的参数一模一样（`--profile` 在
# ags 里是 resume 的参数）。这是这个约定最要紧的性质：Agent 将来新增任何参数都不会
# 被 ags 抢走。
#
# 断言看的是 `AGS_LAUNCH_ARGS` 而不是 argv：这个假 codex 会像真 codex 一样把开头的
# `--profile <名字>` 自己消费掉（见上面那段 `shift 2`），所以它的 argv 打印出来是空
# 的。argv 为空恰恰证明参数到达了 Agent 并被 Agent 吃掉，而不是被 ags 吞了。
fresh_codex_literal_option="$(
    env "${managed_env[@]}" "$tool" codex -- --profile native
)"
grep -Fqx 'FAKE_CODEX' <<< "$fresh_codex_literal_option"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--profile","native"]' \
    <<< "$fresh_codex_literal_option"

fresh_claude="$(
    env "${managed_env[@]}" "$tool" claude -- --model sonnet
)"
grep -Fqx 'FAKE_CLAUDE <--model> <sonnet>' <<< "$fresh_claude"
grep -Eq '^LAUNCH=.*/claude --model sonnet$' \
    "$tmp/home/.local/ags.log"
fresh_claude_owned_args="$(
    env "${managed_env[@]}" "$tool" claude -- --to codex --model sonnet
)"
grep -Fqx \
    'FAKE_CLAUDE <--to> <codex> <--model> <sonnet>' \
    <<< "$fresh_claude_owned_args"
# `--settings` 会被 ags 校验（它限制能注入哪些 Claude 设置，是有意的安全边界），
# 所以不带值的 `--settings` 必须被拒。以前写成 `-- --settings` 时那个字面量 `--`
# 会让校验提前停下——那其实是个绕过口子，`--` 变成分隔符之后它没了。
if env "${managed_env[@]}" "$tool" claude -- --settings >/dev/null 2>&1; then
    printf 'a valueless --settings must still be refused\n' >&2
    exit 1
fi
fresh_claude_literal_option="$(
    env "${managed_env[@]}" "$tool" claude -- --model sonnet
)"
grep -Fqx \
    'FAKE_CLAUDE <--model> <sonnet>' \
    <<< "$fresh_claude_literal_option"
# Claude's selection value is optional, so the word after one of these flags
# belongs to it only when that word is not the next flag.
resumed_claude="$(
    env "${managed_env[@]}" "$tool" claude -- --resume abc123 --model sonnet
)"
grep -Fqx 'FAKE_CLAUDE <--resume> <abc123> <--model> <sonnet>' <<< "$resumed_claude"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","sonnet"]' <<< "$resumed_claude"
resumed_claude_bare="$(
    env "${managed_env[@]}" "$tool" claude -- -r --model sonnet
)"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","sonnet"]' <<< "$resumed_claude_bare"
# `--fork-session` selects a session too: it resumes into a new id, so the
# restored one would be loaded and then abandoned.
resumed_claude_fork="$(
    env "${managed_env[@]}" "$tool" claude -- --continue --fork-session --model sonnet
)"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","sonnet"]' <<< "$resumed_claude_fork"

# A launch reports what the last finished update check found, and reports it out
# of a file: nothing between typing `ags claude` and the Agent starting touches
# the network. With no record there is nothing to say.
rm -f -- "$tmp/state/update-check.lines" "$tmp/state/update-check.stamp"
quiet_claude="$(
    env "${managed_env[@]}" "$tool" claude -- --model sonnet \
        2> "$tmp/quiet-claude.err"
)"
grep -Fqx 'FAKE_CLAUDE <--model> <sonnet>' <<< "$quiet_claude"
! grep -Fq 'update available:' "$tmp/quiet-claude.err"
printf '%s\n' \
    'update available: ags 0.3.0-test -> v0.4.0' \
    'update available: codext 0.146.0 -> v0.147.0' \
    'rm -rf /' \
    > "$tmp/state/update-check.lines"
date +%s > "$tmp/state/update-check.stamp"
update_stamp_before="$(<"$tmp/state/update-check.stamp")"
announced_claude="$(
    env "${managed_env[@]}" "$tool" claude -- --model sonnet \
        2> "$tmp/announced-claude.err"
)"
grep -Fqx 'FAKE_CLAUDE <--model> <sonnet>' <<< "$announced_claude"
grep -Fqx '[ags] update available: ags 0.3.0-test -> v0.4.0' \
    "$tmp/announced-claude.err"
grep -Fqx '[ags] update available: codext 0.146.0 -> v0.147.0' \
    "$tmp/announced-claude.err"
# Only the shape the refresh writes is printed, so a record that picked up
# anything else cannot put arbitrary text behind the `[ags]` prefix.
! grep -Fq 'rm -rf /' "$tmp/announced-claude.err"
# Inside the interval nothing is re-checked and the record is left alone.
[[ "$(<"$tmp/state/update-check.stamp")" == "$update_stamp_before" ]]
env "${managed_env[@]}" AGS_UPDATE_CHECK=0 "$tool" claude -- --model sonnet \
    > /dev/null 2> "$tmp/silenced-claude.err"
! grep -Fq 'update available:' "$tmp/silenced-claude.err"
[[ "$(<"$tmp/state/update-check.stamp")" == "$update_stamp_before" ]]
# Past the interval the launch starts the next check and hands the terminal over
# without waiting for it, so what it prints is still the previous answer. The
# stamp moves before that check runs, which is what keeps several terminals
# opened at once from each starting one.
printf '0\n' > "$tmp/state/update-check.stamp"
stale_claude="$(
    env "${managed_env[@]}" "$tool" claude -- --model sonnet \
        2> "$tmp/stale-claude.err"
)"
grep -Fqx 'FAKE_CLAUDE <--model> <sonnet>' <<< "$stale_claude"
grep -Fqx '[ags] update available: ags 0.3.0-test -> v0.4.0' \
    "$tmp/stale-claude.err"
[[ "$(<"$tmp/state/update-check.stamp")" != 0 ]]

# 不启动 Agent 的命令同样要去拿答案。
#
# 报告（offer_pending_update）本来就在每条命令上跑，刷新却只挂在启动 Agent 那条
# 路上。两边不对称的后果不是"晚一点提醒"，是**永远不提醒**：一台只用 `ags list` /
# `ags resume` / `ags sync` 的机器，那个缓存文件从头到尾没人写过。stamp 被写动就是
# 刷新被排上了——它走后台，所以这条命令自己不等它。
rm -f -- "$tmp/state/update-check.stamp"
env "${managed_env[@]}" "$tool" storage list > /dev/null 2>&1 || true
[[ -s "$tmp/state/update-check.stamp" ]]
[[ "$(<"$tmp/state/update-check.stamp")" != 0 ]]

# 实时查过之后，缓存里那条陈旧提示要被丢掉。
#
# 不丢的后果很具体：`ags codext-update` 报 already current，紧接着 `ags codex` 又问
# "现在更新？"——因为后者读的是缓存，而缓存要等 86400 秒才刷新。用户看到的是"更新
# 根本没生效"。
printf '%s\n' \
    'update available: ags 0.3.0-test -> v0.4.0' \
    'update available: codext 0.146.0 -> v0.147.0' \
    > "$tmp/state/update-check.lines"
env "${managed_env[@]}" "$tool" codext-update > /dev/null 2>&1 || true
# codext 那条没了……
! grep -Fq 'update available: codext' "$tmp/state/update-check.lines"
# ……而 ags 那条还在：它仍然有效，不该被连坐。
grep -Fq 'update available: ags 0.3.0-test' "$tmp/state/update-check.lines"
rm -f -- "$tmp/state/update-check.lines"

# `update` 系列自己就要联网，不该再排一次后台检查。
rm -f -- "$tmp/state/update-check.stamp"
env "${managed_env[@]}" AGS_UPDATE_CHECK_INTERVAL=0 "$tool" codext-update \
    > /dev/null 2>&1 || true
[[ ! -e "$tmp/state/update-check.stamp" ]]

rm -f -- "$tmp/state/update-check.lines" "$tmp/state/update-check.stamp"
safe_claude_settings="$tmp/safe-claude.settings.json"
jq -n '{
  env:{
    ANTHROPIC_BASE_URL:"https://provider.example.test",
    ANTHROPIC_API_KEY:"",
    ANTHROPIC_AUTH_TOKEN:""
  },
  apiKeyHelper:"/usr/bin/printenv TEST_CLAUDE_API_KEY",
  model:"provider-model",
  effortLevel:"high"
}' > "$safe_claude_settings"
safe_claude_settings_output="$(
    env "${managed_env[@]}" "$tool" claude -- \
        --settings "$safe_claude_settings" --model sonnet
)"
grep -Fqx \
    "FAKE_CLAUDE <--settings> <$safe_claude_settings> <--model> <sonnet>" \
    <<< "$safe_claude_settings_output"
dash_claude_settings="$tmp/--safe-claude.settings.json"
cp -- "$safe_claude_settings" "$dash_claude_settings"
dash_claude_settings_output="$(
    cd -- "$tmp"
    env "${managed_env[@]}" "$tool" claude -- \
        --settings --safe-claude.settings.json --model opus
)"
grep -Fqx \
    'FAKE_CLAUDE <--settings> <--safe-claude.settings.json> <--model> <opus>' \
    <<< "$dash_claude_settings_output"
inline_claude_settings='{"env":{"ANTHROPIC_BASE_URL":"https://provider.example.test"}}'
inline_claude_settings_output="$(
    env "${managed_env[@]}" "$tool" claude -- \
        "--settings=$inline_claude_settings" --model haiku
)"
grep -Fqx \
    "FAKE_CLAUDE <--settings=$inline_claude_settings> <--model> <haiku>" \
    <<< "$inline_claude_settings_output"
unsafe_claude_settings="$tmp/unsafe-claude.settings.json"
jq -n '{enabledPlugins:{"some-plugin@example":false}}' \
    > "$unsafe_claude_settings"
launch_count_before="$(grep -c '^LAUNCH=' \
    "$tmp/home/.local/ags.log" || true)"
if env "${managed_env[@]}" "$tool" claude -- \
    --settings "$unsafe_claude_settings" \
    >"$tmp/unsafe-claude-settings.out" \
    2>"$tmp/unsafe-claude-settings.err"; then
    echo 'managed Claude accepted settings that disable the AGS plugin' >&2
    exit 1
fi
grep -Fq 'Claude --settings may contain only' \
    "$tmp/unsafe-claude-settings.err"
launch_count_after="$(grep -c '^LAUNCH=' \
    "$tmp/home/.local/ags.log" || true)"
[[ "$launch_count_after" == "$launch_count_before" ]]

assert_managed_claude_settings_rejected() {
    local label="$1" settings="$2" expected="$3" forbidden="${4:-}"
    launch_count_before="$(
        grep -c '^LAUNCH=' "$tmp/home/.local/ags.log" || true
    )"
    if env "${managed_env[@]}" "$tool" claude -- --settings "$settings" \
        >"$tmp/claude-settings-$label.out" \
        2>"$tmp/claude-settings-$label.err"; then
        echo "managed Claude accepted unsafe settings: $label" >&2
        exit 1
    fi
    grep -Fq "$expected" "$tmp/claude-settings-$label.err"
    if [[ -n "$forbidden" ]] &&
       grep -Fq "$forbidden" "$tmp/claude-settings-$label.err"; then
        echo "managed Claude leaked rejected settings: $label" >&2
        exit 1
    fi
    launch_count_after="$(
        grep -c '^LAUNCH=' "$tmp/home/.local/ags.log" || true
    )"
    [[ "$launch_count_after" == "$launch_count_before" ]]
}

multi_document_claude_settings="$tmp/multi-document-claude.settings.json"
printf '%s\n%s\n' \
    '{"enabledPlugins":{"some-plugin@example":false}}' \
    '{"env":{}}' > "$multi_document_claude_settings"
assert_managed_claude_settings_rejected \
    multiple-json-documents "$multi_document_claude_settings" \
    'Claude --settings may contain only'
null_claude_settings="$tmp/null-claude.settings.json"
printf '%s\n' '{"apiKeyHelper":null}' > "$null_claude_settings"
assert_managed_claude_settings_rejected \
    null-helper "$null_claude_settings" \
    'Claude --settings may contain only'
false_claude_settings="$tmp/false-claude.settings.json"
printf '%s\n' '{"apiKeyHelper":false}' > "$false_claude_settings"
assert_managed_claude_settings_rejected \
    false-helper "$false_claude_settings" \
    'Claude --settings may contain only'
extra_env_claude_settings="$tmp/extra-env-claude.settings.json"
printf '%s\n' '{"env":{"CLAUDE_CODE_SIMPLE":"1"}}' \
    > "$extra_env_claude_settings"
assert_managed_claude_settings_rejected \
    extra-env "$extra_env_claude_settings" \
    'Claude --settings may contain only'
symlink_claude_settings="$tmp/symlink-claude.settings.json"
ln -s -- "$safe_claude_settings" "$symlink_claude_settings"
assert_managed_claude_settings_rejected \
    symlink "$symlink_claude_settings" \
    'Claude --settings cannot be a symbolic link'
inline_claude_secret='AGS_REJECTED_SETTINGS_SECRET_71C9'
assert_managed_claude_settings_rejected \
    inline-permissions \
    "{\"env\":{\"ANTHROPIC_API_KEY\":\"$inline_claude_secret\"},\"permissions\":{}}" \
    'Claude --settings must be a readable JSON file or inline JSON' \
    "$inline_claude_secret"
unsafe_helper_claude_settings="$tmp/unsafe-helper-claude.settings.json"
helper_side_effect="$tmp/helper-side-effect"
jq -n --arg command "touch $helper_side_effect; /usr/bin/printenv TEST_CLAUDE_API_KEY" \
    '{apiKeyHelper:$command}' > "$unsafe_helper_claude_settings"
assert_managed_claude_settings_rejected \
    unsafe-helper "$unsafe_helper_claude_settings" \
    'Claude --settings may contain only'
[[ ! -e "$helper_side_effect" ]]

assert_managed_claude_source_settings_rejected() {
    local label="$1" workdir="$2" expected="$3" forbidden="${4:-}"
    launch_count_before="$(
        grep -c '^LAUNCH=' "$tmp/home/.local/ags.log" || true
    )"
    if (
        cd -- "$workdir"
        env "${managed_env[@]}" "$tool" claude -- --model source-settings-test
    ) >"$tmp/claude-source-settings-$label.out" \
      2>"$tmp/claude-source-settings-$label.err"; then
        echo "managed Claude accepted unsafe source settings: $label" >&2
        exit 1
    fi
    grep -Fq "$expected" "$tmp/claude-source-settings-$label.err"
    if [[ -n "$forbidden" ]] &&
       grep -Fq "$forbidden" "$tmp/claude-source-settings-$label.err"; then
        echo "managed Claude leaked rejected source settings: $label" >&2
        exit 1
    fi
    launch_count_after="$(
        grep -c '^LAUNCH=' "$tmp/home/.local/ags.log" || true
    )"
    [[ "$launch_count_after" == "$launch_count_before" ]]
}

claude_settings_repo="$tmp/claude-settings-repo"
mkdir -p "$claude_settings_repo/subdir" "$claude_settings_repo/.claude"
git -C "$claude_settings_repo" init -q -b main
git -C "$claude_settings_repo" -c user.name=AGS \
    -c user.email=ags@example.invalid commit --allow-empty -qm initial
base_helper_side_effect="$tmp/base-helper-side-effect"
base_helper_secret='AGS_BASE_HELPER_SECRET_4EF2'
jq -n --arg command \
    "if [ \"\${ANTHROPIC_API_KEY:-}\" = ags-settings-probe ]; then :; else touch $base_helper_side_effect; fi; printf '%s' $base_helper_secret" \
    '{apiKeyHelper:$command}' > "$tmp/source/claude/settings.json"
assert_managed_claude_source_settings_rejected \
    conditional-user-helper "$claude_settings_repo/subdir" \
    'Claude user settings must be one JSON object and apiKeyHelper' \
    "$base_helper_secret"
[[ ! -e "$base_helper_side_effect" ]]
rm -f -- "$tmp/source/claude/settings.json"

jq -n --arg command "touch $base_helper_side_effect" \
    '{apiKeyHelper:$command}' \
    > "$claude_settings_repo/.claude/settings.json"
assert_managed_claude_source_settings_rejected \
    project-helper "$claude_settings_repo/subdir" \
    'Claude project settings must be one JSON object and apiKeyHelper'
[[ ! -e "$base_helper_side_effect" ]]
rm -f -- "$claude_settings_repo/.claude/settings.json"

printf '%s\n' '{"apiKeyHelper":null}' \
    > "$claude_settings_repo/.claude/settings.local.json"
assert_managed_claude_source_settings_rejected \
    local-null-helper "$claude_settings_repo/subdir" \
    'Claude local settings must be one JSON object and apiKeyHelper'
rm -f -- "$claude_settings_repo/.claude/settings.local.json"

mkdir -p "$claude_settings_repo/subdir/.claude"
jq -n --arg command "touch $base_helper_side_effect" \
    '{apiKeyHelper:$command}' \
    > "$claude_settings_repo/subdir/.claude/settings.local.json"
assert_managed_claude_source_settings_rejected \
    legacy-local-helper "$claude_settings_repo/subdir" \
    'Claude legacy local settings must be one JSON object and apiKeyHelper'
[[ ! -e "$base_helper_side_effect" ]]
rm -f -- "$claude_settings_repo/subdir/.claude/settings.local.json"

claude_settings_worktree="$tmp/claude-settings-worktree"
git -C "$claude_settings_repo" worktree add -q \
    -b settings-worktree "$claude_settings_worktree"
jq -n --arg command "touch $base_helper_side_effect" \
    '{apiKeyHelper:$command}' \
    > "$claude_settings_repo/.claude/settings.local.json"
assert_managed_claude_source_settings_rejected \
    main-worktree-local-helper "$claude_settings_worktree" \
    'Claude main-worktree local settings must be one JSON object and apiKeyHelper'
[[ ! -e "$base_helper_side_effect" ]]
rm -f -- "$claude_settings_repo/.claude/settings.local.json"

managed_settings_fixture="$tmp/managed-claude.settings.json"
managed_settings_tool="$tmp/ags-managed-settings-test"
managed_helper_side_effect="$tmp/managed-helper-side-effect"
managed_path_occurrences="$(
    grep -Fo '"/etc/claude-code/managed-settings.json"' "$tool" |
        wc -l | tr -d '[:space:]'
)"
[[ "$managed_path_occurrences" == 1 ]]
jq -n --arg command "touch $managed_helper_side_effect" \
    '{apiKeyHelper:$command}' > "$managed_settings_fixture"
sed "s|\"/etc/claude-code/managed-settings.json\"|\"$managed_settings_fixture\"|" \
    "$tool" > "$managed_settings_tool"
chmod +x "$managed_settings_tool"
launch_count_before="$(grep -c '^LAUNCH=' \
    "$tmp/home/.local/ags.log" || true)"
if (
    cd -- "$claude_settings_repo/subdir"
    env "${managed_env[@]}" "$managed_settings_tool" claude -- \
        --model managed-settings-test
) > "$tmp/claude-source-settings-managed.out" \
  2> "$tmp/claude-source-settings-managed.err"; then
    echo 'managed Claude accepted unsafe managed settings' >&2
    exit 1
fi
grep -Fq 'Claude managed settings must be one JSON object and apiKeyHelper' \
    "$tmp/claude-source-settings-managed.err"
launch_count_after="$(grep -c '^LAUNCH=' \
    "$tmp/home/.local/ags.log" || true)"
[[ "$launch_count_after" == "$launch_count_before" ]]
[[ ! -e "$managed_helper_side_effect" ]]

printf '%s\n%s\n' '{}' '{}' > "$tmp/source/claude/settings.json"
assert_managed_claude_source_settings_rejected \
    user-multiple-json "$claude_settings_repo/subdir" \
    'Claude user settings must be one JSON object and apiKeyHelper'
rm -f -- "$tmp/source/claude/settings.json"

ln -s -- "$safe_claude_settings" "$tmp/source/claude/settings.json"
assert_managed_claude_source_settings_rejected \
    user-symlink "$claude_settings_repo/subdir" \
    'Claude user settings cannot be a symbolic link'
rm -f -- "$tmp/source/claude/settings.json"

jq -n '{
  permissions:{deny:["Read(./secret)"]},
  enabledPlugins:{"other-plugin@example":true},
  apiKeyHelper:"/usr/bin/printenv TEST_CLAUDE_API_KEY"
}' > "$tmp/source/claude/settings.json"
safe_source_settings_output="$(
    cd -- "$claude_settings_repo/subdir"
    env "${managed_env[@]}" "$tool" claude -- --model safe-source-settings
)"
grep -Fqx 'FAKE_CLAUDE <--model> <safe-source-settings>' \
    <<< "$safe_source_settings_output"
rm -f -- "$tmp/source/claude/settings.json"


assert_managed_claude_arg_rejected() {
    local label="$1" expected="$2"
    shift 2
    launch_count_before="$(
        grep -c '^LAUNCH=' "$tmp/home/.local/ags.log" || true
    )"
    if env "${managed_env[@]}" "$tool" claude -- "$@" \
        >"$tmp/claude-arg-$label.out" \
        2>"$tmp/claude-arg-$label.err"; then
        echo "managed Claude accepted a forbidden argument: $label" >&2
        exit 1
    fi
    grep -Fq "Claude $expected cannot be forwarded" \
        "$tmp/claude-arg-$label.err"
    launch_count_after="$(
        grep -c '^LAUNCH=' "$tmp/home/.local/ags.log" || true
    )"
    [[ "$launch_count_after" == "$launch_count_before" ]]
}

assert_managed_claude_arg_rejected \
    plugin-dir --plugin-dir --plugin-dir "$tmp/context-shadow"
assert_managed_claude_arg_rejected \
    plugin-dir-equals --plugin-dir "--plugin-dir=$tmp/context-shadow"
assert_managed_claude_arg_rejected \
    plugin-url --plugin-url --plugin-url https://example.invalid/plugin.zip
assert_managed_claude_arg_rejected \
    plugin-url-equals --plugin-url \
    --plugin-url=https://example.invalid/plugin.zip
assert_managed_claude_arg_rejected \
    disallowed-tools --disallowedTools \
    --disallowedTools mcp__some_plugin__do_thing
assert_managed_claude_arg_rejected \
    disallowed-tools-kebab-equals --disallowed-tools \
    --disallowed-tools=mcp__some_plugin__find_thing
assert_managed_claude_arg_rejected \
    agent --agent --agent restricted
assert_managed_claude_arg_rejected \
    agent-equals --agent --agent=restricted
assert_managed_claude_arg_rejected \
    agents --agents --agents '{"restricted":{"tools":["Bash"]}}'
assert_managed_claude_arg_rejected \
    agents-equals --agents '--agents={"restricted":{"tools":["Bash"]}}'
assert_managed_claude_arg_rejected \
    no-session-persistence --no-session-persistence --no-session-persistence
assert_managed_claude_arg_rejected \
    no-session-persistence-equals --no-session-persistence \
    --no-session-persistence=true


extract_checkpoint() {
    local archive="$1" destination="$2"
    rm -rf -- "$destination"
    mkdir -p "$destination"
    age -d -i "$tmp/key" "$archive" | tar -xzf - -C "$destination"
}

assert_format4_archive() {
    local archive="$1" destination="$2"
    local expected_count expected_size expected_index count=0 size=0
    local digest artifact_size relative mode mtime extra artifact
    extract_checkpoint "$archive" "$destination"
    grep -Fqx 'format=4' "$destination/manifest"
    grep -Fqx 'capture_fidelity=full' "$destination/manifest"
    grep -Fqx 'artifact_metadata=mode-mtime-v1' "$destination/manifest"
    for field in binary_invoked_path binary_real_path binary_version binary_sha256 \
        binary_os binary_arch artifact_count artifact_size artifact_index_sha256; do
        grep -Eq "^$field=.+$" "$destination/manifest"
    done
    expected_count="$(sed -n 's/^artifact_count=//p' "$destination/manifest")"
    expected_size="$(sed -n 's/^artifact_size=//p' "$destination/manifest")"
    expected_index="$(sed -n 's/^artifact_index_sha256=//p' "$destination/manifest")"
    [[ "$expected_index" == "$(sha256sum "$destination/artifacts.tsv" | cut -d' ' -f1)" ]]
    while IFS=$'\t' read -r digest artifact_size relative mode mtime extra; do
        [[ "$digest" =~ ^[0-9a-f]{64}$ && "$artifact_size" =~ ^[0-9]+$ ]]
        [[ -n "$relative" && "$mode" =~ ^[0-7]{3}$ &&
           "$mtime" =~ ^(0|[1-9][0-9]{0,11})$ && -z "$extra" ]]
        artifact="$destination/artifacts/$relative"
        [[ -f "$artifact" ]]
        [[ "$artifact_size" == "$(stat -c %s "$artifact")" ]]
        [[ "$digest" == "$(sha256sum "$artifact" | cut -d' ' -f1)" ]]
        count=$((count + 1))
        size=$((size + 10#$artifact_size))
    done < "$destination/artifacts.tsv"
    [[ "$count" == "$expected_count" && "$size" == "$expected_size" ]]
    [[ "$(find "$destination/artifacts" -type f | wc -l)" == "$expected_count" ]]
}

reference_ascii_claude_project_key() {
    local input="$1" sanitized= character code index
    local hash=0 signed_hash absolute_hash suffix= digit
    local alphabet=0123456789abcdefghijklmnopqrstuvwxyz
    local LC_ALL=C
    for ((index = 0; index < ${#input}; index++)); do
        character="${input:index:1}"
        case "$character" in
            [A-Za-z0-9]) sanitized+="$character" ;;
            *) sanitized+=- ;;
        esac
        printf -v code '%d' "'$character"
        hash=$(((hash * 31 + code) & 4294967295))
    done
    if (( ${#sanitized} <= 200 )); then
        printf '%s\n' "$sanitized"
        return
    fi
    if (( hash >= 2147483648 )); then
        signed_hash=$((hash - 4294967296))
    else
        signed_hash="$hash"
    fi
    if (( signed_hash < 0 )); then
        absolute_hash=$((-signed_hash))
    else
        absolute_hash="$signed_hash"
    fi
    if (( absolute_hash == 0 )); then
        suffix=0
    else
        while (( absolute_hash > 0 )); do
            digit=$((absolute_hash % 36))
            suffix="${alphabet:digit:1}$suffix"
            absolute_hash=$((absolute_hash / 36))
        done
    fi
    printf '%s-%s\n' "${sanitized:0:200}" "$suffix"
}

run_clean_init() {
    local init_home="$1"
    shift
    env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
        -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
        -u XDG_CONFIG_HOME -u XDG_DATA_HOME -u XDG_STATE_HOME \
        HOME="$init_home" \
        CODEX_HOME="$init_home/.codex" \
        CLAUDE_CONFIG_DIR="$init_home/.claude" \
        PATH="$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
        "$tool" init "$@"
}


init_home="$tmp/init-home"
init_vault="$init_home/.local/share/ags/checkpoints"
init_identity="$init_home/.config/ags/identity.agekey"
init_config="$init_home/.local/state/ags/storage.json"
mkdir -p "$init_home"
(
    umask 000
    run_clean_init "$init_home"
) > "$tmp/init.out" 2> "$tmp/init.err"
grep -Fqx 'status=initialized' "$tmp/init.out"
grep -Fqx "vault=$init_vault" "$tmp/init.out"
grep -Fqx "identity=$init_identity" "$tmp/init.out"
grep -Fqx "recipient=$(age-keygen -y "$init_identity")" "$tmp/init.out"
grep -Fq 'Back up this identity separately' "$tmp/init.err"
if grep -Fq 'AGE-SECRET-KEY-' "$tmp/init.out" "$tmp/init.err"; then
    echo 'init printed the secret identity' >&2
    exit 1
fi
[[ -d "$init_vault" && -f "$init_identity" && -f "$init_config" ]]
[[ "$(stat -c %a "$init_home/.config/ags")" == 700 ]]
[[ "$(stat -c %a "$init_home/.local/state/ags")" == 700 ]]
[[ "$(stat -c %a "$init_vault")" == 700 ]]
[[ "$(stat -c %a "$init_identity")" == 600 ]]
[[ "$(stat -c %a "$init_config")" == 600 ]]
jq -e --arg vault "$init_vault" --arg identity "$init_identity" '
    .version == 4 and .local_path == $vault and
    .encryption == {type:"age-x25519", identity_file:$identity}
' "$init_config" >/dev/null
init_identity_sha="$(sha256sum "$init_identity" | cut -d' ' -f1)"
run_clean_init "$init_home" > "$tmp/init-again.out" 2> "$tmp/init-again.err"
[[ "$init_identity_sha" == "$(sha256sum "$init_identity" | cut -d' ' -f1)" ]]
grep -Fqx 'status=initialized' "$tmp/init-again.out"

symlink_init_home="$tmp/symlink-init-home"
symlink_init_outside="$tmp/symlink-init-outside"
mkdir -p "$symlink_init_home/.local/share/ags" "$symlink_init_outside"
chmod 755 "$symlink_init_outside"
ln -s "$symlink_init_outside" \
    "$symlink_init_home/.local/share/ags/checkpoints"
symlink_init_mode="$(stat -c %a "$symlink_init_outside")"
if run_clean_init "$symlink_init_home" \
    >"$tmp/symlink-init.out" 2>"$tmp/symlink-init.err"; then
    echo 'init accepted a symbolic-link vault' >&2
    exit 1
fi
grep -Fq 'storage path cannot contain symbolic links' "$tmp/symlink-init.err"
[[ "$(stat -c %a "$symlink_init_outside")" == "$symlink_init_mode" ]]
[[ -z "$(find "$symlink_init_outside" -mindepth 1 -print -quit)" ]]

migration_home="$tmp/migration-home"
migration_vault="$tmp/migration-vault"
migration_config="$migration_home/.local/state/ags/storage.json"
mkdir -p "$migration_vault" "$(dirname "$migration_config")"
jq -n --arg vault "$migration_vault" '{
    version:3,
    local_path:$vault,
    remotes:{origin:{type:"git", url:"test://origin", branch:"main"}},
    cloud:{url:"sftp://tester@example.test:22/ags", auth:"agent"}
}' > "$migration_config"
run_clean_init "$migration_home" > "$tmp/migration-init.out" 2> "$tmp/migration-init.err"
jq -e --arg vault "$migration_vault" \
    --arg identity "$migration_home/.config/ags/identity.agekey" '
    .local_path == $vault and
    .version == 4 and
    .encryption.identity_file == $identity and
    .remotes.origin.url == "test://origin" and
    .cloud.auth == "agent"
' "$migration_config" >/dev/null

configured_home="$tmp/configured-identity-home"
configured_vault="$tmp/configured-identity-vault"
configured_identity="$tmp/configured-identity.agekey"
configured_other_identity="$tmp/configured-other-identity.agekey"
configured_config="$configured_home/.local/state/ags/storage.json"
age-keygen -o "$configured_identity" >/dev/null 2>&1
age-keygen -o "$configured_other_identity" >/dev/null 2>&1
mkdir -p "$configured_vault" "$(dirname "$configured_config")"
jq -n --arg vault "$configured_vault" --arg identity "$configured_identity" '{
    version:3,
    local_path:$vault,
    encryption:{type:"age-x25519", identity_file:$identity}
}' > "$configured_config"
run_clean_init "$configured_home" > "$tmp/configured-init.out" 2> "$tmp/configured-init.err"
grep -Fqx "identity=$configured_identity" "$tmp/configured-init.out"
jq -e --arg identity "$configured_identity" \
    '.version == 4 and .encryption.identity_file == $identity' \
    "$configured_config" >/dev/null
[[ ! -e "$configured_home/.config/ags/identity.agekey" ]]
if run_clean_init "$configured_home" --identity "$configured_other_identity" \
    >"$tmp/configured-conflict.out" 2>"$tmp/configured-conflict.err"; then
    echo 'init replaced a configured identity' >&2
    exit 1
fi
grep -Fq 'refusing to replace the configured identity' "$tmp/configured-conflict.err"
jq -e --arg identity "$configured_identity" \
    '.encryption.identity_file == $identity' "$configured_config" >/dev/null

import_source="$tmp/import-source.agekey"
age-keygen -o "$import_source" >/dev/null 2>&1
chmod 640 "$import_source"
import_source_sha="$(sha256sum "$import_source" | cut -d' ' -f1)"
import_home="$tmp/import-home"
run_clean_init "$import_home" --identity "$import_source" \
    > "$tmp/import-init.out" 2> "$tmp/import-init.err"
import_identity="$import_home/.config/ags/identity.agekey"
cmp "$import_source" "$import_identity"
[[ "$(stat -c %a "$import_source")" == 640 ]]
[[ "$(stat -c %a "$import_identity")" == 600 ]]
[[ "$import_source_sha" == "$(sha256sum "$import_source" | cut -d' ' -f1)" ]]
run_clean_init "$import_home" --identity "$import_source" >/dev/null 2>&1
[[ "$import_source_sha" == "$(sha256sum "$import_identity" | cut -d' ' -f1)" ]]
different_identity="$tmp/different-identity.agekey"
age-keygen -o "$different_identity" >/dev/null 2>&1
if run_clean_init "$import_home" --identity "$different_identity" \
    >"$tmp/import-conflict.out" 2>"$tmp/import-conflict.err"; then
    echo 'init overwrote an existing identity' >&2
    exit 1
fi
grep -Fq 'refusing to replace the existing identity' "$tmp/import-conflict.err"
[[ "$import_source_sha" == "$(sha256sum "$import_identity" | cut -d' ' -f1)" ]]
printf 'not an age identity\n' > "$tmp/invalid-identity"
invalid_home="$tmp/invalid-init-home"
if run_clean_init "$invalid_home" --identity "$tmp/invalid-identity" \
    >"$tmp/invalid-init.out" 2>"$tmp/invalid-init.err"; then
    echo 'init accepted an invalid age identity' >&2
    exit 1
fi
[[ ! -e "$invalid_home/.config/ags/identity.agekey" ]]
[[ ! -e "$invalid_home/.local/state/ags/storage.json" ]]
if run_clean_init "$tmp/relative-init-home" --identity relative.agekey \
    >/dev/null 2>&1; then
    echo 'init accepted a relative identity path' >&2
    exit 1
fi

age_session_id=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee
age_session_rel="sessions/2026/01/01/rollout-age-$age_session_id.jsonl"
age_codex_home="$tmp/age-source/codex"
mkdir -p "$age_codex_home/$(dirname "$age_session_rel")" "$tmp/age-work"
printf '%s\n' '{"type":"session_meta"}' '{"type":"event_msg"}' \
    > "$age_codex_home/$age_session_rel"
age_env=(
    HOME="$init_home"
    PATH="$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin"
    CODEX_HOME="$age_codex_home"
    CLAUDE_CONFIG_DIR="$tmp/age-source/claude"
    CODEX_THREAD_ID=
    CLAUDE_CODE_SESSION_ID=
    AGENT_SESSION_DISABLE_GEO=1
)
age_checkpoint_output="$(
    cd "$tmp/age-work"
    env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
        -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
        "${age_env[@]}" "$tool" save-now local codex "$age_session_id" \
        age-only 'Native age identity'
)"
age_checkpoint_path="$(sed -n 's/^path=//p' <<< "$age_checkpoint_output")"
age -d -i "$init_identity" "$age_checkpoint_path" | tar -tzf - | grep -Fqx manifest
env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
    -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
    "${age_env[@]}" "$tool" list | grep -Fq 'Native age identity'
env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
    -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
    "${age_env[@]}" "$tool" show age-only | grep -Eq '^ID +age-only$'
age_resume="$(
    cd "$tmp/age-work"
    env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
        -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
        "${age_env[@]}" "$tool" resume age-only -- --model test-model
)"
grep -Fq "FAKE_CODEX <resume> <$age_session_id> <--model> <test-model>" <<< "$age_resume"

identity_priority_output="$(
    cd "$tmp/age-work"
    env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_LOCAL_DIR \
        -u AGENT_SESSION_STATE_DIR "${age_env[@]}" \
        AGENT_SESSION_SSH_KEY="$tmp/key" \
        "$tool" save-now local codex "$age_session_id" identity-priority \
        'Configured identity wins over SFTP key'
)"
identity_priority_path="$(sed -n 's/^path=//p' <<< "$identity_priority_output")"
age -d -i "$init_identity" "$identity_priority_path" | tar -tzf - | grep -Fqx manifest
if age -d -i "$tmp/key" "$identity_priority_path" >/dev/null 2>&1; then
    echo 'new record was encrypted to AGENT_SESSION_SSH_KEY after init' >&2
    exit 1
fi
env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_LOCAL_DIR \
    -u AGENT_SESSION_STATE_DIR "${age_env[@]}" \
    AGENT_SESSION_SSH_KEY="$tmp/key" "$tool" show identity-priority |
    grep -Eq '^ID +identity-priority$'

legacy_init_home="$tmp/legacy-init-home"
legacy_init_vault="$tmp/legacy-init-vault"
mkdir -p "$legacy_init_home/.ssh"
install -m600 "$tmp/key" "$legacy_init_home/.ssh/id_ed25519"
legacy_init_env=(
    HOME="$legacy_init_home"
    PATH="$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin"
    CODEX_HOME="$age_codex_home"
    CLAUDE_CONFIG_DIR="$tmp/age-source/claude"
    CODEX_THREAD_ID=
    CLAUDE_CODE_SESSION_ID=
    AGENT_SESSION_DISABLE_GEO=1
)
env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
    -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
    "${legacy_init_env[@]}" "$tool" set "$legacy_init_vault" >/dev/null
(
    cd "$tmp/age-work"
    env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
        -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
        "${legacy_init_env[@]}" "$tool" save-now local codex "$age_session_id" \
        legacy-before-init 'Legacy SSH identity'
) >/dev/null
run_clean_init "$legacy_init_home" >/dev/null 2>&1
env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
    -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
    "${legacy_init_env[@]}" "$tool" list | grep -Fq 'Legacy SSH identity'
env -u AGENT_SESSION_IDENTITY_FILE -u AGENT_SESSION_SSH_KEY \
    -u AGENT_SESSION_LOCAL_DIR -u AGENT_SESSION_STATE_DIR \
    "${legacy_init_env[@]}" "$tool" show legacy-before-init |
    grep -Eq '^ID +legacy-before-init$'

if env "${source_env[@]}" "$tool" list >/dev/null 2>&1; then
    echo 'list accepted missing storage configuration' >&2
    exit 1
fi
if env "${source_env[@]}" "$tool" set relative/path >/dev/null 2>&1; then
    echo 'set accepted a relative path' >&2
    exit 1
fi
if env "${source_env[@]}" "$tool" set /mnt/c >/dev/null 2>&1; then
    echo 'set accepted a Windows mount' >&2
    exit 1
fi
set_symlink_root="$tmp/set-symlink-root"
set_symlink_outside="$tmp/set-symlink-outside"
mkdir -p "$set_symlink_root" "$set_symlink_outside"
chmod 755 "$set_symlink_outside"
ln -s "$set_symlink_outside" "$set_symlink_root/linked"
set_symlink_mode="$(stat -c %a "$set_symlink_outside")"
if env "${source_env[@]}" "$tool" set "$set_symlink_root/linked/vault" \
    >"$tmp/set-symlink.out" 2>"$tmp/set-symlink.err"; then
    echo 'set accepted an intermediate symbolic link' >&2
    exit 1
fi
grep -Fq 'storage path cannot contain symbolic links' "$tmp/set-symlink.err"
[[ "$(stat -c %a "$set_symlink_outside")" == "$set_symlink_mode" ]]
[[ ! -e "$set_symlink_outside/vault" ]]
local_config="$(env "${source_env[@]}" "$tool" set "$tmp/local-checkpoints")"
grep -Fqx 'status=configured' <<< "$local_config"
grep -Fqx "path=$tmp/local-checkpoints" <<< "$local_config"
if env "${source_env[@]}" "$tool" local "$tmp/local-checkpoints" >/dev/null 2>&1; then
    echo 'legacy local command is still accepted' >&2
    exit 1
fi

env "${source_env[@]}" "$tool" legacy history push
env "${source_env[@]}" "$tool" legacy history status | grep -Fq '.sessions.tar.gz.age'
env "${target_env[@]}" "$tool" legacy history pull
cmp "$tmp/source/codex/sessions/2026/01/01/test.jsonl" \
    "$tmp/target/codex/sessions/2026/01/01/test.jsonl"

printf '%s\n' '{"turn":2}' >> "$tmp/source/codex/sessions/2026/01/01/test.jsonl"
env "${source_env[@]}" "$tool" legacy history push
env "${target_env[@]}" "$tool" legacy history pull
cmp "$tmp/source/codex/sessions/2026/01/01/test.jsonl" \
    "$tmp/target/codex/sessions/2026/01/01/test.jsonl"

printf '%s\n' '{"branch":"local"}' >> "$tmp/target/codex/sessions/2026/01/01/test.jsonl"
printf '%s\n' '{"branch":"remote"}' >> "$tmp/source/codex/sessions/2026/01/01/test.jsonl"
env "${source_env[@]}" "$tool" legacy history push
before="$(sha256sum "$tmp/target/codex/sessions/2026/01/01/test.jsonl")"
env "${target_env[@]}" "$tool" legacy history pull >/dev/null 2>&1 && exit 1
[[ "$before" == "$(sha256sum "$tmp/target/codex/sessions/2026/01/01/test.jsonl")" ]]

session_id=11111111-2222-4333-8444-555555555555
session_rel="sessions/2026/01/01/rollout-test-$session_id.jsonl"
mkdir -p "$tmp/source/codex/$(dirname "$session_rel")" "$tmp/work"
printf '%s\n' '{"type":"session_meta"}' '{"type":"event_msg"}' > "$tmp/source/codex/$session_rel"
codex_mode=640
codex_mtime=1700000001
chmod "$codex_mode" "$tmp/source/codex/$session_rel"
touch -d "@$codex_mtime" -- "$tmp/source/codex/$session_rel"
checkpoint_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" sat-index 'SAT 抽取索引'
)"
checkpoint_id="$(sed -n 's/^checkpoint_id=//p' <<< "$checkpoint_output")"
checkpoint_record_id="$(sed -n 's/^record_id=//p' <<< "$checkpoint_output")"
checkpoint_path="$(sed -n 's/^path=//p' <<< "$checkpoint_output")"
[[ "$checkpoint_id" == sat-index ]]
[[ "$checkpoint_record_id" =~ ^[0-9a-f]{24}@sat-index$ ]]
grep -Fqx 'description=SAT 抽取索引' <<< "$checkpoint_output"
grep -Fqx 'agent=codex' <<< "$checkpoint_output"
grep -Fqx "agent_binary=$tmp/home/.local/bin/codex" <<< "$checkpoint_output"
grep -Fqx 'binary_resolution=path' <<< "$checkpoint_output"
grep -Fqx "pwd=$tmp/work" <<< "$checkpoint_output"
grep -Fqx 'public_ip=203.0.113.7' <<< "$checkpoint_output"
grep -Fqx 'ip_location=Test City, Test Region, Test Country [1.25, 2.5]' <<< "$checkpoint_output"
geo_disabled_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_DISABLE_GEO=1 \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" geo-disabled 'Geo disabled'
)"
grep -Fqx 'public_ip=unknown' <<< "$geo_disabled_output"
grep -Fqx 'ip_location=unknown' <<< "$geo_disabled_output"
layout_description='终端对齐检查：Long descriptions stay in the final column without wrapping.'
layout_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_DISABLE_GEO=1 \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" list-layout "$layout_description"
)"
grep -Fqx "description=$layout_description" <<< "$layout_output"
layout_record_id="$(sed -n 's/^record_id=//p' <<< "$layout_output")"

# A session saved with Agent arguments. The environment is where they come from,
# because that is what `ags codex` exports before exec and what `ags save`
# inherits from inside the Agent.
launch_args_session=22222222-3333-4444-8555-666666666666
launch_args_rel="sessions/2026/01/02/rollout-test-$launch_args_session.jsonl"
mkdir -p "$tmp/source/codex/$(dirname "$launch_args_rel")"
printf '%s\n' '{"type":"session_meta"}' '{"type":"event_msg"}' \
    > "$tmp/source/codex/$launch_args_rel"
launch_args_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_DISABLE_GEO=1 \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        AGS_LAUNCH_ARGS='["--yolo","--model","o3 mini"]' \
        "$tool" save-now local codex "$launch_args_session" with-args '带启动参数的会话'
)"
launch_args_record_id="$(sed -n 's/^record_id=//p' <<< "$launch_args_output")"
launch_args_path="$(sed -n 's/^path=//p' <<< "$launch_args_output")"
# The variable is inherited, so it can hold anything. A value that does not
# decode records no arguments rather than reaching the manifest.
launch_args_junk="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_DISABLE_GEO=1 \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        AGS_LAUNCH_ARGS='not json at all' \
        "$tool" save-now local codex "$launch_args_session" args-junk 'Junk in the environment'
)"
grep -Fqx 'checkpoint_id=args-junk' <<< "$launch_args_junk"
grep -Eq '^Agent arguments +none$' <<< "$(
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" show args-junk
)"
# Saving records what the session ran with; whether those arguments are allowed
# is decided at launch. Kept in its own store so the resume below is the only
# thing that can select it — and the store has to exist first, because
# `AGENT_SESSION_LOCAL_DIR` names a directory rather than creating one.
mkdir -p "$tmp/forbidden-args-checkpoints"
(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_DISABLE_GEO=1 \
        AGENT_SESSION_LOCAL_DIR="$tmp/forbidden-args-checkpoints" \
        AGS_LAUNCH_ARGS='["--add-dir","/tmp"]' \
        "$tool" save-now local codex "$launch_args_session" with-args 'Forbidden argument'
) > /dev/null
assert_format4_archive "$checkpoint_path" "$tmp/extracted/codex"
grep -Fqx "agent=codex" "$tmp/extracted/codex/manifest"
grep -Fqx "relative_path=$session_rel" "$tmp/extracted/codex/manifest"
grep -Fqx "binary_invoked_path=$tmp/home/.local/bin/codex" \
    "$tmp/extracted/codex/manifest"
grep -Fqx "binary_real_path=$tmp/home/.local/bin/codex" \
    "$tmp/extracted/codex/manifest"
grep -Fqx 'binary_version=codex-test 1.0' "$tmp/extracted/codex/manifest"
grep -Fqx 'binary_resolution=path' "$tmp/extracted/codex/manifest"
grep -Fqx "binary_sha256=$(sha256sum "$tmp/home/.local/bin/codex" | cut -d' ' -f1)" \
    "$tmp/extracted/codex/manifest"
cmp "$tmp/source/codex/$session_rel" "$tmp/extracted/codex/artifacts/$session_rel"
grep -Fqx \
    "$(sha256sum "$tmp/source/codex/$session_rel" | cut -d' ' -f1)"$'\t'"$(stat -c %s "$tmp/source/codex/$session_rel")"$'\t'"$session_rel"$'\t'"$codex_mode"$'\t'"$codex_mtime" \
    "$tmp/extracted/codex/artifacts.tsv"

checkpoint_list="$(env "${source_env[@]}" "$tool" list)"
grep -Eq '^ID +AGENT +SAVED +DESCRIPTION$' <<< "$checkpoint_list"
[[ "$checkpoint_list" != *$'\t'* ]]
grep -Eq '^sat-index +CODEX +[^ ]+ +SAT 抽取索引$' <<< "$checkpoint_list"
[[ "$checkpoint_list" != *"$checkpoint_record_id"* ]]
[[ "$checkpoint_list" != *"$tmp/home/.local/bin/codex"* ]]
[[ "$checkpoint_list" != *"$tmp/work"* ]]
for terminal_width in 80 120; do
    width_list="$(env "${source_env[@]}" COLUMNS="$terminal_width" "$tool" list)"
    (( $(LC_ALL="$test_utf8_locale" wc -L <<< "$width_list") <= terminal_width ))
    [[ "$(grep -Ec '^ID +AGENT +SAVED +DESCRIPTION$' <<< "$width_list")" == 1 ]]
    [[ "$(grep -Ec '^list-layout +CODEX +' <<< "$width_list")" == 1 ]]
done

if [[ "$test_platform" == Linux && -x /usr/bin/script ]]; then
    cat > "$tmp/stty-guard" <<'EOF'
#!/usr/bin/env bash
set -u
before="$(stty -g)"
"$@"
child_status=$?
after="$(stty -g)"
[[ "$before" == "$after" ]] || exit 90
printf 'STTY_UNCHANGED\n'
exit "$child_status"
EOF
    chmod +x "$tmp/stty-guard"
    run_checkpoint_pty() {
        local input="$1" output="$2" checkpoint_root="$3" pty_command
        shift 3
        printf -v pty_command '%q ' \
            env "${source_env[@]}" \
            AGENT_SESSION_LOCAL_DIR="$checkpoint_root" "$@" \
            "$tmp/stty-guard" "$tool" list
        printf '%s' "$input" | \
            SHELL=/bin/bash /usr/bin/script -q -e -f -c "$pty_command" /dev/null \
                > "$output" 2>&1
    }

    # Defined before the first assertion that needs it. Everything the picker
    # writes under a pseudo-terminal is styled, so a fixed-string match against
    # what it "says" only ever works on the stripped frame.
    #
    # The third expression is not redundant with the first. A pseudo-terminal
    # echoes what was typed, and these tests type bare escapes — a lone \e that
    # begins no CSI sequence survives the first rule and lands at the head of
    # the first line the picker draws. It costs nothing in a `grep -F`, but it
    # is counted by `wc -L`, which is how a frame that is exactly COLUMNS wide
    # measured 102 against a limit of 100.
    strip_terminal_control() {
        sed -e 's/\x1b\[[?0-9;]*[a-zA-Z]//g' -e 's/\r//g' -e 's/\x1b//g'
    }

    mkdir -p "$tmp/tab-state" "$tmp/tab-checkpoints"
    tab_env=(
        "${managed_env[@]}"
        AGENT_SESSION_STATE_DIR="$tmp/tab-state"
        AGENT_SESSION_LOCAL_DIR="$tmp/tab-checkpoints"
    )
    run_agent_pty() {
        local input="$1" output="$2" launch_dir="$3" terminal_type="$4"
        local pty_command
        shift 4
        printf -v pty_command '%q ' \
            env "${tab_env[@]}" TERM="$terminal_type" \
            "$tmp/stty-guard" "$tool" "$@"
        (
            cd "$launch_dir"
            printf '%s' "$input" | \
                SHELL=/bin/bash /usr/bin/script -q -e -f \
                    -c "$pty_command" /dev/null > "$output" 2>&1
        )
    }

    checkpoint_root_before="$(
        find "$tmp/local-checkpoints" -type f -print0 |
            LC_ALL=C sort -z | xargs -0 sha256sum
    )"
    run_checkpoint_pty $'\033' "$tmp/checkpoint-escape.out" \
        "$tmp/local-checkpoints"
    run_checkpoint_pty $'\177\033' "$tmp/checkpoint-backspace.out" \
        "$tmp/local-checkpoints"
    run_checkpoint_pty $'\033[3~n\033' "$tmp/checkpoint-delete-no.out" \
        "$tmp/local-checkpoints"
    delete_frame="$(strip_terminal_control < "$tmp/checkpoint-delete-no.out")"
    grep -Fq 'Delete this saved session? [y/N]' <<< "$delete_frame"
    [[ "$checkpoint_root_before" == "$(
        find "$tmp/local-checkpoints" -type f -print0 |
            LC_ALL=C sort -z | xargs -0 sha256sum
    )" ]]

    args_menu_root="$tmp/args-menu-checkpoints"
    mkdir -p "$args_menu_root/codex"
    cp -- "$launch_args_path" \
        "$args_menu_root/codex/$launch_args_record_id.checkpoint.tar.gz.age"

    # The picker draws through the same renderer the piped table uses, so every
    # row comes back padded to one display width. Only the frame can show that:
    # the piped table is not padded, and a Chinese description is two columns
    # per character, which is exactly what a byte-counted layout gets wrong.
    run_checkpoint_pty $'\033' "$tmp/checkpoint-args-frame.out" \
        "$args_menu_root" COLUMNS=100 LINES=30
    args_frame="$(strip_terminal_control < "$tmp/checkpoint-args-frame.out")"
    grep -Eq '^  ID +AGENT +SAVED +DESCRIPTION *$' <<< "$args_frame"
    grep -Eq '^. with-args +CODEX +[0-9-]+ +带启动参数的会话 *$' <<< "$args_frame"
    # A "no line exceeds COLUMNS" assertion stood here and never passed. What it
    # measured was not the frame: a pseudo-terminal interleaves its own echo of
    # the keystrokes with the program's output, so the first drawn line carries
    # leading bytes this test typed rather than anything the picker chose to
    # write. Stripping the escapes is not enough to separate the two, and the
    # padding check immediately below already asserts the layout claim that
    # matters — that every row is padded to one display width — without
    # depending on where the echo landed.
    # The heading is ASCII and the row ends in Chinese. Equal *display* width is
    # therefore the whole claim: a byte- or character-counted layout gets this
    # pair wrong by exactly the number of wide characters in the description.
    [[ "$(
        grep -E '^  ID +AGENT' <<< "$args_frame" |
            LC_ALL="$test_utf8_locale" wc -L
      )" == "$(
        grep -E '^. with-args +CODEX' <<< "$args_frame" |
            LC_ALL="$test_utf8_locale" wc -L
      )" ]]
    # The saved arguments are shown before anything is launched with them.
    grep -Fq "args  --yolo --model 'o3 mini'" <<< "$args_frame"

    # `a` replaces them for this launch, and Enter opens with what was typed.
    # Ctrl-U first: the prompt opens prefilled with what is saved. The trailing
    # newline answers the workspace question below, which fires here because the
    # session was saved in "$tmp/work" and this runs somewhere else.
    run_checkpoint_pty $'a\025--model o3\n\n\n' "$tmp/checkpoint-args-edit.out" \
        "$args_menu_root" CODEX_HOME="$tmp/args-menu-target/codex"
    args_edit_frame="$(strip_terminal_control < "$tmp/checkpoint-args-edit.out")"
    grep -Fq "FAKE_CODEX <resume> <$launch_args_session> <--model> <o3>" \
        <<< "$args_edit_frame"
    # Opening a session is not a report. The restore record still goes to a
    # caller capturing it — the non-terminal resumes above read those fields —
    # but it must not land between the picker and the Agent's own banner.
    ! grep -Eq '^(checkpoint_id|capture_fidelity|restore_pwd|conflicts)=' \
        <<< "$args_edit_frame"

    # Clearing the line means no arguments, not "fall back to the saved ones".
    run_checkpoint_pty $'a\025\n\n\n' "$tmp/checkpoint-args-cleared.out" \
        "$args_menu_root" CODEX_HOME="$tmp/args-menu-cleared/codex"
    args_cleared_frame="$(strip_terminal_control < "$tmp/checkpoint-args-cleared.out")"
    grep -Fq "FAKE_CODEX <resume> <$launch_args_session>" <<< "$args_cleared_frame"
    ! grep -Fq '<--yolo>' <<< "$args_cleared_frame"

    # Resuming somewhere other than where a session was saved is a change of
    # workspace, so it is asked rather than assumed. This checkpoint was saved
    # in "$tmp/work" and the picker runs here, so the two paths differ and the
    # question fires; both answers are honoured.
    pty_cwd="$(pwd -P)"
    [[ "$pty_cwd" != "$tmp/work" ]]
    run_checkpoint_pty $'\n2\n' "$tmp/checkpoint-cwd-saved.out" \
        "$args_menu_root" CODEX_HOME="$tmp/cwd-saved-target/codex"
    cwd_saved_frame="$(strip_terminal_control < "$tmp/checkpoint-cwd-saved.out")"
    grep -Fq 'This session was saved in a different directory' \
        <<< "$cwd_saved_frame"
    grep -Fq "$tmp/work" <<< "$cwd_saved_frame"
    grep -Fqx "FAKE_PWD=$tmp/work" <<< "$cwd_saved_frame"

    run_checkpoint_pty $'\n1\n' "$tmp/checkpoint-cwd-here.out" \
        "$args_menu_root" CODEX_HOME="$tmp/cwd-here-target/codex"
    cwd_here_frame="$(strip_terminal_control < "$tmp/checkpoint-cwd-here.out")"
    grep -Fqx "FAKE_PWD=$pty_cwd" <<< "$cwd_here_frame"
    grep -Fq $'\033]0;with-args\007' "$tmp/checkpoint-cwd-here.out"

    # Same directory, nothing to choose between: the question must not appear.
    cwd_same_out="$(
        cd "$tmp/work"
        env "${source_env[@]}" CODEX_HOME="$tmp/cwd-same-target/codex" \
            AGENT_SESSION_LOCAL_DIR="$args_menu_root" \
            "$tmp/stty-guard" "$tool" resume with-args
    )"
    ! grep -Fq 'saved in a different directory' <<< "$cwd_same_out"
    grep -Fqx "FAKE_PWD=$tmp/work" <<< "$cwd_same_out"

    unsafe_selector_root="$tmp/unsafe-checkpoint-selector"
    unsafe_selector_id=$'0000\033]52;c;AGS_CHECKPOINT\a'
    mkdir -p "$unsafe_selector_root/codex"
    cp -- "$checkpoint_path" \
        "$unsafe_selector_root/codex/$unsafe_selector_id.checkpoint.tar.gz.age"
    run_checkpoint_pty $'\033[3~n\033' "$tmp/checkpoint-unsafe-selector.out" \
        "$unsafe_selector_root"
    grep -Fq 'no local checkpoints' "$tmp/checkpoint-unsafe-selector.out"
    if LC_ALL=C grep -Fq ']52;c;AGS_CHECKPOINT' \
        "$tmp/checkpoint-unsafe-selector.out"; then
        echo 'checkpoint selector emitted an unsafe terminal control payload' >&2
        exit 1
    fi

    ui_checkpoint_root="$tmp/ui-checkpoints"
    mkdir -p "$ui_checkpoint_root/codex"
    cp -- "$checkpoint_path" \
        "$ui_checkpoint_root/codex/$checkpoint_record_id.checkpoint.tar.gz.age"
    run_checkpoint_pty $'\033[3~y' "$tmp/checkpoint-delete-yes.out" \
        "$ui_checkpoint_root"
    [[ ! -e "$ui_checkpoint_root/codex/$checkpoint_record_id.checkpoint.tar.gz.age" ]]
    [[ -f "$ui_checkpoint_root/tombstones/codex/$checkpoint_record_id.tombstone" ]]
    grep -Fq 'it remains recoverable' "$tmp/checkpoint-delete-yes.out"

    # A genuinely new interactive launch asks once. Enter accepts the current
    # directory name, a typed name wins, and duplicate names need no special
    # handling because terminal titles are not identifiers.
    run_agent_pty $'focused work\n' "$tmp/tab-custom.out" \
        "$tmp/work" xterm-256color codex --model o3
    grep -Fq 'Terminal tab name [work]:' "$tmp/tab-custom.out"
    grep -Fq $'\033]0;focused work\007' "$tmp/tab-custom.out"
    run_agent_pty $'\n' "$tmp/tab-default.out" \
        "$tmp/work" xterm-256color codex --model o3
    grep -Fq $'\033]0;work\007' "$tmp/tab-default.out"

    # A native resume with an explicit id does not ask a new-session question.
    # TERM=dumb represents terminals that cannot consume the title sequence:
    # those launches remain completely untouched.
    native_tab_id=99999999-8888-4777-8666-555555555555
    run_agent_pty '' "$tmp/tab-resume.out" \
        "$tmp/work" xterm-256color codex resume "$native_tab_id"
    ! grep -Fq 'Terminal tab name' "$tmp/tab-resume.out"
    grep -Fq $'\033]0;'"$native_tab_id"$'\007' "$tmp/tab-resume.out"
    run_agent_pty '' "$tmp/tab-dumb.out" \
        "$tmp/work" dumb codex --model o3
    ! grep -Fq 'Terminal tab name' "$tmp/tab-dumb.out"
    ! grep -Fq $'\033]0;' "$tmp/tab-dumb.out"
fi

checkpoint_show="$(env "${source_env[@]}" "$tool" show "$checkpoint_id")"
for expected in \
    "^ID +$checkpoint_id$" \
    "^Record +$checkpoint_record_id$" \
    '^Agent +codex$' \
    "^Native session +$session_id$" \
    '^Description +SAT 抽取索引$' \
    "^Workspace +$tmp/work$" \
    '^Completeness +full$' \
    '^Artifacts +1 files, [0-9]+ bytes$' \
    '^Artifact metadata +mode-mtime-v1$' \
    "^Binary invoked +$tmp/home/.local/bin/codex$" \
    "^Binary real +$tmp/home/.local/bin/codex$" \
    '^Binary version +codex-test 1.0$' \
    '^Binary source +path$' \
    '^Binary SHA-256 +[0-9a-f]{64}$' \
    '^Binary selected +.*/codex$' \
    '^Public IP +203.0.113.7$' \
    '^Archive SHA-256 +[0-9a-f]{64}$' \
    '^Local integrity +verified$' \
    "^Path +$checkpoint_path$"; do
    grep -Eq "$expected" <<< "$checkpoint_show"
done
restore_output="$(env "${source_env[@]}" CODEX_HOME="$tmp/checkpoint-target/codex" \
    CLAUDE_CONFIG_DIR="$tmp/checkpoint-target/claude" \
    AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" "$tool" restore local "$checkpoint_id")"
grep -Fqx "saved_pwd=$tmp/work" <<< "$restore_output"
grep -Fqx 'public_ip=203.0.113.7' <<< "$restore_output"
cmp "$tmp/source/codex/$session_rel" "$tmp/checkpoint-target/codex/$session_rel"
[[ "$(stat -c '%a:%Y' "$tmp/checkpoint-target/codex/$session_rel")" == \
   "$codex_mode:$codex_mtime" ]]
write_codex_profile "$tmp/checkpoint-record-target/codex" exact-record
record_resume="$(env "${source_env[@]}" CODEX_HOME="$tmp/checkpoint-record-target/codex" \
    "$tool" resume "$checkpoint_record_id" --profile=exact-record)"
grep -Fqx "FAKE_CODEX <resume> <$session_id> <--profile> <exact-record>" <<< "$record_resume"
cmp "$tmp/source/codex/$session_rel" "$tmp/checkpoint-record-target/codex/$session_rel"

# The saved arguments come back, and they are announced rather than applied
# silently — these records synchronize, so this can be another machine's
# command line running here.
resume_saved_args="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/launch-args-target/codex" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" resume with-args 2> "$tmp/resume-saved-args.err"
)"
grep -Fqx "FAKE_CODEX <resume> <$launch_args_session> <--yolo> <--model> <o3 mini>" \
    <<< "$resume_saved_args"
grep -Fq "codex arguments: --yolo --model 'o3 mini'" "$tmp/resume-saved-args.err"
# ...and they carry into the resumed session, so the next save records the
# command line this session is really running under instead of losing it here.
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--yolo","--model","o3 mini"]' <<< "$resume_saved_args"

# A record written before AGS dropped the selection still carries it, and
# replaying `resume <other-id>` after the id this resume just restored would
# open that other session instead. It is dropped here too, and said out loud.
mkdir -p "$tmp/stale-args-checkpoints"
(
    cd "$tmp/work"
    env "${source_env[@]}" \
        AGENT_SESSION_DISABLE_GEO=1 \
        AGENT_SESSION_LOCAL_DIR="$tmp/stale-args-checkpoints" \
        AGS_LAUNCH_ARGS='["resume","01999999-aaaa-7bbb-8ccc-dddddddddddd","--last","--model","o3"]' \
        "$tool" save-now local codex "$launch_args_session" stale-args '恢复中保存的会话'
) > /dev/null
resume_stale_args="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/stale-args-target/codex" \
        AGENT_SESSION_LOCAL_DIR="$tmp/stale-args-checkpoints" \
        "$tool" resume stale-args 2> "$tmp/resume-stale-args.err"
)"
grep -Fqx "FAKE_CODEX <resume> <$launch_args_session> <--model> <o3>" \
    <<< "$resume_stale_args"
grep -Fq 'dropped native session selection from codex arguments' \
    "$tmp/resume-stale-args.err"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=["--model","o3"]' <<< "$resume_stale_args"
# A typed selection loses the same way, so the picker's editable argument line
# cannot put one back either.
resume_typed_selection="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/typed-selection-target/codex" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" resume with-args -- resume --last --model o3 \
        2> "$tmp/resume-typed-selection.err"
)"
grep -Fqx "FAKE_CODEX <resume> <$launch_args_session> <--model> <o3>" \
    <<< "$resume_typed_selection"
grep -Fq 'dropped native session selection from codex arguments' \
    "$tmp/resume-typed-selection.err"

# `--` with nothing after it is a decision, not an omission: run this session
# with no Agent arguments. Without that distinction a cleared argument line in
# the picker would be refilled from the checkpoint it was cleared on.
resume_cleared_args="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/launch-args-cleared/codex" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" resume with-args --
)"
grep -Fqx "FAKE_CODEX <resume> <$launch_args_session>" <<< "$resume_cleared_args"
grep -Fqx 'FAKE_AGS_LAUNCH_ARGS=[]' <<< "$resume_cleared_args"

resume_override_args="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/launch-args-override/codex" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" resume with-args -- --model o3
)"
grep -Fqx "FAKE_CODEX <resume> <$launch_args_session> <--model> <o3>" \
    <<< "$resume_override_args"

# Replayed arguments face the same argument checks a typed one does. A
# synchronized record must not be a way around it.
env "${source_env[@]}" CODEX_HOME="$tmp/launch-args-forbidden/codex" \
    AGENT_SESSION_LOCAL_DIR="$tmp/forbidden-args-checkpoints" \
    "$tool" resume with-args > "$tmp/forbidden-args.out" 2> "$tmp/forbidden-args.err" && exit 1
grep -Fq 'cannot be forwarded' "$tmp/forbidden-args.err"
mkdir -p "$tmp/symlinked-codex-home-real"
ln -s "$tmp/symlinked-codex-home-real" "$tmp/symlinked-codex-home"
env "${source_env[@]}" CODEX_HOME="$tmp/symlinked-codex-home" \
    AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
    "$tool" restore local "$checkpoint_id" >/dev/null
[[ -L "$tmp/symlinked-codex-home" ]]
cmp "$tmp/source/codex/$session_rel" "$tmp/symlinked-codex-home-real/$session_rel"
[[ "$(stat -c '%a:%Y' "$tmp/symlinked-codex-home-real/$session_rel")" == \
   "$codex_mode:$codex_mtime" ]]

compat_payload="$tmp/format4-three-column/payload"
compat_checkpoints="$tmp/format4-three-column/checkpoints"
mkdir -p "$compat_payload" "$compat_checkpoints/codex"
cp -a "$tmp/extracted/codex/." "$compat_payload/"
cut -f1-3 "$compat_payload/artifacts.tsv" > "$compat_payload/artifacts.tsv.new"
mv -- "$compat_payload/artifacts.tsv.new" "$compat_payload/artifacts.tsv"
sed '/^artifact_metadata=/d' "$compat_payload/manifest" \
    > "$compat_payload/manifest.new"
mv -- "$compat_payload/manifest.new" "$compat_payload/manifest"
compat_index_sha="$(sha256sum "$compat_payload/artifacts.tsv" | cut -d' ' -f1)"
sed "s/^artifact_index_sha256=.*/artifact_index_sha256=$compat_index_sha/" \
    "$compat_payload/manifest" > "$compat_payload/manifest.new"
mv -- "$compat_payload/manifest.new" "$compat_payload/manifest"
ssh-keygen -y -f "$tmp/key" > "$tmp/format4-three-column/recipient.pub"
tar -C "$compat_payload" -czf - manifest artifacts.tsv artifacts | \
    age -R "$tmp/format4-three-column/recipient.pub" \
        -o "$compat_checkpoints/codex/$checkpoint_record_id.checkpoint.tar.gz.age"
env "${source_env[@]}" CODEX_HOME="$tmp/format4-three-column/target" \
    AGENT_SESSION_LOCAL_DIR="$compat_checkpoints" \
    "$tool" restore local "$checkpoint_id" >/dev/null
cmp "$tmp/source/codex/$session_rel" "$tmp/format4-three-column/target/$session_rel"
[[ "$(stat -c %a "$tmp/format4-three-column/target/$session_rel")" == 600 ]]
env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$compat_checkpoints" \
    "$tool" show "$checkpoint_id" | grep -Eq '^Artifact metadata +not captured$'

unsafe_metadata_payload="$tmp/unsafe-metadata/payload"
unsafe_metadata_checkpoints="$tmp/unsafe-metadata/checkpoints"
mkdir -p "$unsafe_metadata_payload" "$unsafe_metadata_checkpoints/codex"
cp -a "$tmp/extracted/codex/." "$unsafe_metadata_payload/"
awk -F '\t' 'BEGIN { OFS="\t" } { $4="4755"; print }' \
    "$unsafe_metadata_payload/artifacts.tsv" > "$unsafe_metadata_payload/artifacts.tsv.new"
mv -- "$unsafe_metadata_payload/artifacts.tsv.new" "$unsafe_metadata_payload/artifacts.tsv"
unsafe_metadata_index_sha="$(sha256sum "$unsafe_metadata_payload/artifacts.tsv" | cut -d' ' -f1)"
sed "s/^artifact_index_sha256=.*/artifact_index_sha256=$unsafe_metadata_index_sha/" \
    "$unsafe_metadata_payload/manifest" > "$unsafe_metadata_payload/manifest.new"
mv -- "$unsafe_metadata_payload/manifest.new" "$unsafe_metadata_payload/manifest"
ssh-keygen -y -f "$tmp/key" > "$tmp/unsafe-metadata/recipient.pub"
tar -C "$unsafe_metadata_payload" -czf - manifest artifacts.tsv artifacts | \
    age -R "$tmp/unsafe-metadata/recipient.pub" \
        -o "$unsafe_metadata_checkpoints/codex/$checkpoint_record_id.checkpoint.tar.gz.age"
if env "${source_env[@]}" CODEX_HOME="$tmp/unsafe-metadata/target" \
    AGENT_SESSION_LOCAL_DIR="$unsafe_metadata_checkpoints" \
    "$tool" restore local "$checkpoint_id" \
    >"$tmp/unsafe-metadata.out" 2>"$tmp/unsafe-metadata.err"; then
    echo 'restore accepted unsafe artifact mode bits' >&2
    exit 1
fi
grep -Fq 'checkpoint artifact metadata is malformed' "$tmp/unsafe-metadata.err"
[[ ! -e "$tmp/unsafe-metadata/target" ]]

zst_session_id=22222222-3333-4444-8555-666666666666
zst_rel="archived_sessions/rollout-test-$zst_session_id.jsonl.zst"
mkdir -p "$tmp/source/codex/$(dirname "$zst_rel")"
printf 'fake-zstd-rollout\0payload\n' > "$tmp/source/codex/$zst_rel"
zst_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$zst_session_id" compressed 'Compressed Codex session'
)"
zst_path="$(sed -n 's/^path=//p' <<< "$zst_output")"
assert_format4_archive "$zst_path" "$tmp/extracted/zst"
grep -Fqx "relative_path=$zst_rel" "$tmp/extracted/zst/manifest"
cmp "$tmp/source/codex/$zst_rel" "$tmp/extracted/zst/artifacts/$zst_rel"
zst_resume="$(env "${source_env[@]}" CODEX_HOME="$tmp/zst-target/codex" \
    "$tool" resume compressed -- --model compressed-model)"
grep -Fqx \
    "FAKE_CODEX <resume> <$zst_session_id> <--model> <compressed-model>" \
    <<< "$zst_resume"
[[ "$zst_resume" != *'<-->'* ]]
cmp "$tmp/source/codex/$zst_rel" "$tmp/zst-target/codex/$zst_rel"
zst_cross_home="$tmp/zst-cross-target/claude"
zst_cross_key="$(LC_ALL=C sed 's/[^A-Za-z0-9]/-/g' <<< "$tmp/work")"
zst_cross="$(env "${source_env[@]}" CLAUDE_CONFIG_DIR="$zst_cross_home" \
    OPENAI_API_KEY=must-not-leak ANTHROPIC_API_KEY=must-not-leak \
    "$tool" resume compressed --to claude -- \
        --model converted-zstd)"
grep -Fqx \
    "FAKE_CLAUDE <--resume> <$converted_claude_id> <--model> <converted-zstd>" \
    <<< "$zst_cross"
[[ -f "$zst_cross_home/projects/$zst_cross_key/$converted_claude_id.jsonl" ]]
grep -Fq $'CONVERT=codex\tclaude-code\t' "$tmp/home/.local/ags.log"
grep -F $'CONVERT=codex\tclaude-code\t' "$tmp/home/.local/ags.log" |
    grep -Fq '/ags/source.jsonl'

(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_STATE_DIR="$tmp/race-state-a" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" local-race 'Local race A'
) >"$tmp/local-race-a.out" 2>"$tmp/local-race-a.err" &
local_race_a_pid=$!
(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_STATE_DIR="$tmp/race-state-b" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" local-race 'Local race B'
) >"$tmp/local-race-b.out" 2>"$tmp/local-race-b.err" &
local_race_b_pid=$!
if wait "$local_race_a_pid"; then local_race_a=0; else local_race_a=$?; fi
if wait "$local_race_b_pid"; then local_race_b=0; else local_race_b=$?; fi
if (( (local_race_a == 0 && local_race_b == 0) ||
      (local_race_a != 0 && local_race_b != 0) )); then
    echo "local race returned unexpected statuses: $local_race_a, $local_race_b" >&2
    exit 1
fi
mapfile -t local_race_records < <(
    sed -n 's/^record_id=//p' "$tmp/local-race-a.out" "$tmp/local-race-b.out"
)
(( ${#local_race_records[@]} == 1 ))
[[ "${local_race_records[0]}" =~ ^[0-9a-f]{24}@local-race$ ]]
[[ -f "$tmp/local-checkpoints/codex/${local_race_records[0]}.checkpoint.tar.gz.age" ]]

delete_move_seed="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" \
            delete-move-failure 'Delete move failure'
)"
delete_move_record="$(sed -n 's/^record_id=//p' <<< "$delete_move_seed")"
delete_move_path="$(sed -n 's/^path=//p' <<< "$delete_move_seed")"
delete_move_failure="$tmp/delete-move-failure.injected"
if env "${source_env[@]}" \
    AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
    FAKE_MV_DELETE_ARCHIVE_ONCE="$delete_move_failure" \
    "$tool" delete "$delete_move_record" \
    >"$tmp/delete-move-failure.out" \
    2>"$tmp/delete-move-failure.err"; then
    echo 'local delete ignored a failed recoverable archive move' >&2
    exit 1
fi
[[ -f "$delete_move_failure" && -f "$delete_move_path" ]]
[[ ! -e "$tmp/local-checkpoints/tombstones/codex/$delete_move_record.tombstone" ]]
grep -Fq 'cannot move checkpoint to recoverable trash' \
    "$tmp/delete-move-failure.err"
delete_move_done="$(
    env "${source_env[@]}" \
        AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" delete "$delete_move_record"
)"
grep -Fqx 'status=deleted' <<< "$delete_move_done"

cloud_url='sftp://tester@127.0.0.1:2222/agent-sessions'
if env "${source_env[@]}" "$tool" cloud set 'https://example.test/sessions' --key "$tmp/key" >/dev/null 2>&1; then
    echo 'cloud set accepted a non-SFTP URL' >&2
    exit 1
fi
cloud_config="$(env "${source_env[@]}" "$tool" cloud set "$cloud_url" --key "$tmp/key")"
grep -Fqx 'status=configured' <<< "$cloud_config"
grep -Fqx "url=$cloud_url" <<< "$cloud_config"
grep -Fqx 'auth=key' <<< "$cloud_config"

if (
    cd "$tmp/work"
    env "${source_env[@]}" FAKE_RCLONE_FAIL_MANIFEST_MOVE_ONCE=1 \
        "$tool" save-now cloud codex "$session_id" rollback-check 'Rollback check'
) >/dev/null 2>&1; then
    echo 'cloud save ignored a failed manifest commit' >&2
    exit 1
fi
! find "$tmp/remote/agent-sessions/codex" -maxdepth 1 -type f \
    -name '*@rollback-check.checkpoint.tar.gz.age' | grep -q .
! find "$tmp/remote/agent-sessions/codex" -maxdepth 1 -type f \
    -name '*@rollback-check.manifest.age' | grep -q .
rollback_retry="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" rollback-check 'Rollback check'
)"
grep -Fqx 'status=saved' <<< "$rollback_retry"
env "${source_env[@]}" "$tool" cloud delete rollback-check >/dev/null

manifest_after_move_marker="$tmp/manifest-after-move.failed"
manifest_after_move="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        FAKE_RCLONE_FAIL_MANIFEST_AFTER_MOVE_ONCE="$manifest_after_move_marker" \
        "$tool" save-now cloud codex "$session_id" \
            manifest-after-move 'Manifest committed before client error'
)"
manifest_after_move_record="$(
    sed -n 's/^record_id=//p' <<< "$manifest_after_move"
)"
grep -Fqx 'status=saved' <<< "$manifest_after_move"
[[ -f "$manifest_after_move_marker" ]]
[[ -f "$tmp/remote/agent-sessions/codex/$manifest_after_move_record.checkpoint.tar.gz.age" ]]
[[ -f "$tmp/remote/agent-sessions/codex/$manifest_after_move_record.manifest.age" ]]
env "${source_env[@]}" "$tool" cloud delete "$manifest_after_move_record" >/dev/null

retained_pending="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_AGENT=codex \
        AGENT_SESSION_ID="$session_id" \
        "$tool" cloud save retained-archive 'Retain reused archive'
)"
retained_record="$(sed -n 's/^record_id=//p' <<< "$retained_pending")"
retained_archive="$tmp/remote/agent-sessions/codex/$retained_record.checkpoint.tar.gz.age"
retained_manifest="$tmp/remote/agent-sessions/codex/$retained_record.manifest.age"
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${source_env[@]}" \
    FAKE_RCLONE_FAIL_MANIFEST_MOVE_ONCE="$tmp/retained-manifest-first.failed" \
    FAKE_RCLONE_FAIL_ARCHIVE_DELETE_ONCE="$tmp/retained-delete.failed" \
    "$tool" hook >"$tmp/retained-first.out" 2>"$tmp/retained-first.err"
grep -Fq "unconfirmed cloud checkpoint retained for safe recovery: codex/$retained_record" \
    "$tmp/retained-first.err"
[[ -f "$retained_archive" && ! -e "$retained_manifest" ]]
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${source_env[@]}" \
    FAKE_RCLONE_FAIL_MANIFEST_MOVE_ONCE="$tmp/retained-manifest-second.failed" \
    "$tool" hook >"$tmp/retained-second.out" 2>"$tmp/retained-second.err"
grep -Fq "unconfirmed cloud checkpoint retained for safe recovery: codex/$retained_record" \
    "$tmp/retained-second.err"
[[ -f "$retained_archive" && ! -e "$retained_manifest" ]]
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${source_env[@]}" "$tool" hook
[[ -f "$retained_archive" && -f "$retained_manifest" ]]
env "${source_env[@]}" "$tool" cloud delete "$retained_record" >/dev/null

lsf_probe_counter="$tmp/rollback-lsf-probe.count"
if (
    cd "$tmp/work"
    env "${source_env[@]}" \
        FAKE_RCLONE_FAIL_MANIFEST_MOVE_ONCE="$tmp/rollback-lsf-manifest.failed" \
        FAKE_RCLONE_FAIL_LSF_AT=2 \
        FAKE_RCLONE_LSF_COUNTER_FILE="$lsf_probe_counter" \
        "$tool" save-now cloud codex "$session_id" \
            rollback-lsf-probe 'Unconfirmed rollback probe'
) >"$tmp/rollback-lsf-probe.out" 2>"$tmp/rollback-lsf-probe.err"; then
    echo 'cloud save ignored an unavailable rollback probe' >&2
    exit 1
fi
grep -Fq 'unconfirmed cloud checkpoint retained for safe recovery: codex/' \
    "$tmp/rollback-lsf-probe.err"
mapfile -t rollback_lsf_archives < <(
    find "$tmp/remote/agent-sessions/codex" -maxdepth 1 -type f \
        -name '*@rollback-lsf-probe.checkpoint.tar.gz.age'
)
(( ${#rollback_lsf_archives[@]} == 1 ))
rollback_lsf_record="${rollback_lsf_archives[0]##*/}"
rollback_lsf_record="${rollback_lsf_record%.checkpoint.tar.gz.age}"
[[ ! -e "$tmp/remote/agent-sessions/codex/$rollback_lsf_record.manifest.age" ]]
env "${source_env[@]}" "$tool" cloud delete "$rollback_lsf_record" >/dev/null

legacy_delete_retry="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" \
            legacy-delete-retry 'Legacy delete retry'
)"
legacy_delete_record="$(
    sed -n 's/^record_id=//p' <<< "$legacy_delete_retry"
)"
legacy_delete_archive="$tmp/remote/agent-sessions/codex/$legacy_delete_record.checkpoint.tar.gz.age"
legacy_delete_manifest="$tmp/remote/agent-sessions/codex/$legacy_delete_record.manifest.age"
legacy_delete_failure="$tmp/legacy-delete-trash-manifest.failed"
if env "${source_env[@]}" \
    FAKE_RCLONE_FAIL_LEGACY_TRASH_MANIFEST_ONCE="$legacy_delete_failure" \
    "$tool" cloud delete legacy-delete-retry \
    >"$tmp/legacy-delete-first.out" \
    2>"$tmp/legacy-delete-first.err"; then
    echo 'legacy cloud delete ignored a failed recoverable manifest move' >&2
    exit 1
fi
[[ -f "$legacy_delete_failure" ]]
[[ -f "$legacy_delete_archive" && -f "$legacy_delete_manifest" ]]
legacy_delete_tombstone="$tmp/local-checkpoints/tombstones/codex/$legacy_delete_record.tombstone"
[[ -f "$legacy_delete_tombstone" ]]
legacy_delete_tombstone_digest="$(
    sha256sum "$legacy_delete_tombstone" | cut -d' ' -f1
)"
legacy_delete_tombstone_marker="ags-v1/tombstones/codex/$legacy_delete_record.$legacy_delete_tombstone_digest.tombstone"
[[ -f "$tmp/remote/agent-sessions/$legacy_delete_tombstone_marker" ]]

# A new checkpoint may legitimately reuse the old logical ID after its
# predecessor is tombstoned. Retrying cleanup of the exact legacy record must
# not delete that newer, unrelated local checkpoint.
legacy_delete_new="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" \
        "$tool" save-now local codex "$session_id" \
            legacy-delete-retry 'New checkpoint with reused logical ID'
)"
legacy_delete_new_record="$(
    sed -n 's/^record_id=//p' <<< "$legacy_delete_new"
)"
legacy_delete_new_path="$(
    sed -n 's/^path=//p' <<< "$legacy_delete_new"
)"
[[ "$legacy_delete_new_record" != "$legacy_delete_record" ]]
legacy_delete_done="$(
    env "${source_env[@]}" "$tool" cloud delete legacy-delete-retry
)"
grep -Fqx 'status=deleted' <<< "$legacy_delete_done"
grep -Fqx 'target=cloud' <<< "$legacy_delete_done"
grep -Fqx "record_id=$legacy_delete_record" <<< "$legacy_delete_done"
[[ -f "$legacy_delete_new_path" ]]
[[ ! -e "$legacy_delete_archive" && ! -e "$legacy_delete_manifest" ]]
find "$tmp/remote/agent-sessions/trash/codex" -maxdepth 1 -type f \
    -name "$legacy_delete_record.checkpoint.tar.gz.age.deleted.*" | grep -q .
find "$tmp/remote/agent-sessions/trash/codex" -maxdepth 1 -type f \
    -name "$legacy_delete_record.manifest.age.deleted.*" | grep -q .

legacy_archive_retry="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" \
            legacy-archive-retry 'Legacy archive move retry'
)"
legacy_archive_record="$(
    sed -n 's/^record_id=//p' <<< "$legacy_archive_retry"
)"
legacy_archive_path="$tmp/remote/agent-sessions/codex/$legacy_archive_record.checkpoint.tar.gz.age"
legacy_archive_manifest="$tmp/remote/agent-sessions/codex/$legacy_archive_record.manifest.age"
legacy_archive_failure="$tmp/legacy-delete-trash-archive.failed"
if env "${source_env[@]}" \
    FAKE_RCLONE_FAIL_LEGACY_TRASH_ARCHIVE_ONCE="$legacy_archive_failure" \
    "$tool" cloud delete legacy-archive-retry \
    >"$tmp/legacy-archive-first.out" \
    2>"$tmp/legacy-archive-first.err"; then
    echo 'legacy cloud delete ignored a failed final archive move' >&2
    exit 1
fi
[[ -f "$legacy_archive_failure" && -f "$legacy_archive_path" ]]
[[ ! -e "$legacy_archive_manifest" ]]
find "$tmp/remote/agent-sessions/trash/codex" -maxdepth 1 -type f \
    -name "$legacy_archive_record.manifest.age.deleted.*" | grep -q .
legacy_archive_done="$(
    env "${source_env[@]}" "$tool" cloud delete legacy-archive-retry
)"
grep -Fqx 'status=deleted' <<< "$legacy_archive_done"
grep -Fqx "record_id=$legacy_archive_record" <<< "$legacy_archive_done"
[[ ! -e "$legacy_archive_path" ]]

queue_publish_retry="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" \
            queue-publish-retry 'Queue publication failure'
)"
queue_publish_record="$(
    sed -n 's/^record_id=//p' <<< "$queue_publish_retry"
)"
queue_publish_archive="$tmp/remote/agent-sessions/codex/$queue_publish_record.checkpoint.tar.gz.age"
queue_publish_manifest="$tmp/remote/agent-sessions/codex/$queue_publish_record.manifest.age"
queue_publish_failure="$tmp/queue-publish-failure.injected"
if queue_publish_first="$(
    env "${source_env[@]}" \
        FAKE_RCLONE_FAIL_LSF_AT=3 \
        FAKE_RCLONE_LSF_COUNTER_FILE="$tmp/queue-publish-lsf.count" \
        FAKE_MV_PENDING_SYNC_ONCE="$queue_publish_failure" \
        "$tool" cloud delete queue-publish-retry \
        2>"$tmp/queue-publish-first.err"
)"; then
    echo 'delete ignored failure to publish its remote retry' >&2
    exit 1
fi
grep -Fqx 'status=deleted' <<< "$queue_publish_first"
grep -Fqx 'target=cloud' <<< "$queue_publish_first"
! grep -Fq 'sync_status=pending' <<< "$queue_publish_first"
grep -Fq 'remote retry could not be recorded' \
    "$tmp/queue-publish-first.err"
[[ -f "$queue_publish_failure" ]]
[[ -f "$queue_publish_archive" && -f "$queue_publish_manifest" ]]
[[ ! -d "$tmp/state/pending-sync" ]] ||
    ! find "$tmp/state/pending-sync" -type f -name '*.json' | grep -q .
env "${source_env[@]}" "$tool" sync neburst >/dev/null
queue_publish_done="$(
    env "${source_env[@]}" "$tool" cloud delete queue-publish-retry
)"
grep -Fqx "record_id=$queue_publish_record" <<< "$queue_publish_done"
[[ ! -e "$queue_publish_archive" && ! -e "$queue_publish_manifest" ]]

legacy_pending_retry="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" \
            legacy-pending-retry 'Legacy pending sync retry'
)"
legacy_pending_record="$(
    sed -n 's/^record_id=//p' <<< "$legacy_pending_retry"
)"
legacy_pending_archive="$tmp/remote/agent-sessions/codex/$legacy_pending_record.checkpoint.tar.gz.age"
legacy_pending_manifest="$tmp/remote/agent-sessions/codex/$legacy_pending_record.manifest.age"
if legacy_pending_first="$(
    env "${source_env[@]}" \
        FAKE_RCLONE_FAIL_LSF_AT=3 \
        FAKE_RCLONE_LSF_COUNTER_FILE="$tmp/legacy-pending-lsf.count" \
        "$tool" cloud delete legacy-pending-retry \
        2>"$tmp/legacy-pending-first.err"
)"; then
    echo 'legacy cloud delete ignored a failed tombstone synchronization' >&2
    exit 1
fi
grep -Fqx 'status=deleted' <<< "$legacy_pending_first"
grep -Fqx 'target=cloud' <<< "$legacy_pending_first"
grep -Fqx 'sync_status=pending' <<< "$legacy_pending_first"
[[ -f "$legacy_pending_archive" && -f "$legacy_pending_manifest" ]]
legacy_pending_sync="$(
    sed -n 's/^pending_sync=//p' <<< "$legacy_pending_first"
)"
[[ -f "$legacy_pending_sync" ]]

flush_race_retry="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" \
            flush-race-retry 'Flush retry replacement race'
)"
flush_race_record="$(
    sed -n 's/^record_id=//p' <<< "$flush_race_retry"
)"
mkdir -p "$tmp/flush-retry-race/hold"
env "${source_env[@]}" \
    FAKE_RM_PENDING_HOLD_DIR="$tmp/flush-retry-race/hold" \
    "$tool" flush \
    >"$tmp/flush-retry-race/flush.out" \
    2>"$tmp/flush-retry-race/flush.err" &
flush_race_flush_pid=$!
test_child_pids["$flush_race_flush_pid"]=1
flush_race_remove_ready=0
for _ in {1..1000}; do
    if [[ -e "$tmp/flush-retry-race/hold/ready" ]]; then
        flush_race_remove_ready=1
        break
    fi
    test_process_running "$flush_race_flush_pid" || break
    sleep 0.01
done
(( flush_race_remove_ready == 1 ))
if ! test_process_has_fd_target "$flush_race_flush_pid" \
    "$tmp/state/storage-consolidation.lock"; then
    : > "$tmp/flush-retry-race/hold/release"
    wait "$flush_race_flush_pid" 2>/dev/null || true
    unset "test_child_pids[$flush_race_flush_pid]"
    echo 'pending retry removal was not protected by the consolidation lock' >&2
    exit 1
fi
env "${source_env[@]}" \
    FAKE_RCLONE_FAIL_LSF_AT=3 \
    FAKE_RCLONE_LSF_COUNTER_FILE="$tmp/flush-retry-race/delete-lsf.count" \
    "$tool" cloud delete flush-race-retry \
    >"$tmp/flush-retry-race/delete.out" \
    2>"$tmp/flush-retry-race/delete.err" &
flush_race_delete_pid=$!
test_child_pids["$flush_race_delete_pid"]=1
flush_race_delete_waiting=0
for _ in {1..500}; do
    if test_process_has_fd_target "$flush_race_delete_pid" \
        "$tmp/state/storage-consolidation.lock"; then
        flush_race_delete_waiting=1
        break
    fi
    test_process_running "$flush_race_delete_pid" || break
    sleep 0.01
done
(( flush_race_delete_waiting == 1 ))
test_process_running "$flush_race_flush_pid"
test_process_running "$flush_race_delete_pid"
: > "$tmp/flush-retry-race/hold/release"
wait "$flush_race_flush_pid"
unset "test_child_pids[$flush_race_flush_pid]"
if wait "$flush_race_delete_pid"; then
    unset "test_child_pids[$flush_race_delete_pid]"
    echo 'replacement retry delete unexpectedly synchronized' >&2
    exit 1
fi
unset "test_child_pids[$flush_race_delete_pid]"
grep -Fqx 'status=deleted' "$tmp/flush-retry-race/delete.out"
grep -Fqx 'sync_status=pending' "$tmp/flush-retry-race/delete.out"
flush_race_pending="$(
    sed -n 's/^pending_sync=//p' "$tmp/flush-retry-race/delete.out"
)"
[[ "$flush_race_pending" == "$legacy_pending_sync" ]]
[[ -f "$flush_race_pending" ]]
jq -e --arg reason "delete:$flush_race_record" \
    '.reason == $reason' "$flush_race_pending" >/dev/null
env "${source_env[@]}" "$tool" flush
[[ ! -e "$flush_race_pending" ]]
flush_race_done="$(
    env "${source_env[@]}" "$tool" cloud delete flush-race-retry
)"
grep -Fqx "record_id=$flush_race_record" <<< "$flush_race_done"

legacy_pending_done="$(
    env "${source_env[@]}" "$tool" cloud delete legacy-pending-retry
)"
grep -Fqx "record_id=$legacy_pending_record" <<< "$legacy_pending_done"
[[ ! -e "$legacy_pending_archive" && ! -e "$legacy_pending_manifest" ]]

cloud_delete_lock="$(
    cd "$tmp/work"
    env "${source_env[@]}" \
        "$tool" save-now cloud codex "$session_id" \
            cloud-delete-lock 'Cloud delete lock'
)"
cloud_delete_lock_record="$(
    sed -n 's/^record_id=//p' <<< "$cloud_delete_lock"
)"
mkdir -p "$tmp/cloud-delete-lock/hold"
env "${source_env[@]}" \
    FAKE_RCLONE_LSF_HOLD_DIR="$tmp/cloud-delete-lock/hold" \
    "$tool" cloud delete "$cloud_delete_lock_record" \
    >"$tmp/cloud-delete-lock/delete.out" \
    2>"$tmp/cloud-delete-lock/delete.err" &
cloud_delete_lock_pid=$!
test_child_pids["$cloud_delete_lock_pid"]=1
cloud_delete_hold_ready=0
for _ in {1..500}; do
    if [[ -e "$tmp/cloud-delete-lock/hold/ready" ]]; then
        cloud_delete_hold_ready=1
        break
    fi
    sleep 0.01
done
(( cloud_delete_hold_ready == 1 ))
env "${source_env[@]}" "$tool" status neburst \
    >"$tmp/cloud-delete-lock/status.out" \
    2>"$tmp/cloud-delete-lock/status.err" &
cloud_delete_status_pid=$!
test_child_pids["$cloud_delete_status_pid"]=1
cloud_delete_status_waiting=0
for _ in {1..500}; do
    if test_process_has_fd_target "$cloud_delete_status_pid" \
        "$tmp/state/storage-consolidation.lock"; then
        cloud_delete_status_waiting=1
        break
    fi
    test_process_running "$cloud_delete_status_pid" || break
    sleep 0.01
done
(( cloud_delete_status_waiting == 1 ))
test_process_running "$cloud_delete_lock_pid"
test_process_running "$cloud_delete_status_pid"
: > "$tmp/cloud-delete-lock/hold/release"
wait "$cloud_delete_lock_pid"
unset "test_child_pids[$cloud_delete_lock_pid]"
wait "$cloud_delete_status_pid"
unset "test_child_pids[$cloud_delete_status_pid]"
grep -Fqx 'status=deleted' "$tmp/cloud-delete-lock/delete.out"
grep -Fqx 'remote=neburst' "$tmp/cloud-delete-lock/status.out"
grep -Fqx 'type=sftp' "$tmp/cloud-delete-lock/status.out"

mkdir -p "$tmp/cloud-race-state-a" "$tmp/cloud-race-state-b"
cp -- "$tmp/state/storage.json" "$tmp/cloud-race-state-a/storage.json"
cp -- "$tmp/state/storage.json" "$tmp/cloud-race-state-b/storage.json"
(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_STATE_DIR="$tmp/cloud-race-state-a" \
        FAKE_RCLONE_LSF_BARRIER="$tmp/cloud-race-barrier" \
        "$tool" save-now cloud codex "$session_id" cloud-race 'Cloud race A'
) >"$tmp/cloud-race-a.out" 2>"$tmp/cloud-race-a.err" &
cloud_race_a_pid=$!
(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_STATE_DIR="$tmp/cloud-race-state-b" \
        FAKE_RCLONE_LSF_BARRIER="$tmp/cloud-race-barrier" \
        "$tool" save-now cloud codex "$session_id" cloud-race 'Cloud race B'
) >"$tmp/cloud-race-b.out" 2>"$tmp/cloud-race-b.err" &
cloud_race_b_pid=$!
if wait "$cloud_race_a_pid"; then cloud_race_a=0; else cloud_race_a=$?; fi
if wait "$cloud_race_b_pid"; then cloud_race_b=0; else cloud_race_b=$?; fi
if (( cloud_race_a != 0 || cloud_race_b != 0 )); then
    echo "cloud race returned unexpected statuses: $cloud_race_a, $cloud_race_b" >&2
    exit 1
fi
mapfile -t cloud_race_records < <(
    sed -n 's/^record_id=//p' "$tmp/cloud-race-a.out" "$tmp/cloud-race-b.out"
)
(( ${#cloud_race_records[@]} == 2 ))
[[ "${cloud_race_records[0]}" != "${cloud_race_records[1]}" ]]
for record_id in "${cloud_race_records[@]}"; do
    [[ "$record_id" =~ ^[0-9a-f]{24}@cloud-race$ ]]
    [[ -f "$tmp/remote/agent-sessions/codex/$record_id.checkpoint.tar.gz.age" ]]
    [[ -f "$tmp/remote/agent-sessions/codex/$record_id.manifest.age" ]]
done
cloud_race_list="$(env "${source_env[@]}" "$tool" cloud list)"
cloud_race_rows="$(grep -Ec '^cloud-race +CODEX +' <<< "$cloud_race_list" || true)"
[[ "$cloud_race_rows" == 2 ]]
if env "${source_env[@]}" CODEX_HOME="$tmp/cloud-race-ambiguous/codex" \
    "$tool" restore cloud cloud-race >"$tmp/cloud-race-ambiguous.out" \
    2>"$tmp/cloud-race-ambiguous.err"; then
    echo 'cloud restore accepted an ambiguous ID' >&2
    exit 1
fi
grep -Fq 'matching RECORD_ID values:' "$tmp/cloud-race-ambiguous.err"
grep -Fq 'expected one cloud checkpoint named cloud-race; found 2' \
    "$tmp/cloud-race-ambiguous.err"
for record_id in "${cloud_race_records[@]}"; do
    grep -Fq "$record_id" "$tmp/cloud-race-ambiguous.err"
done
cloud_race_resume="$(env "${source_env[@]}" CODEX_HOME="$tmp/cloud-race-target/codex" \
    "$tool" cloud resume "${cloud_race_records[0]}" -- --model race-model)"
grep -Fqx "FAKE_CODEX <resume> <$session_id> <--model> <race-model>" <<< "$cloud_race_resume"
cmp "$tmp/source/codex/$session_rel" "$tmp/cloud-race-target/codex/$session_rel"
for record_id in "${cloud_race_records[@]}"; do
    cloud_race_delete="$(env "${source_env[@]}" "$tool" cloud delete "$record_id")"
    grep -Fqx "record_id=$record_id" <<< "$cloud_race_delete"
done

cloud_pending="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
        "$tool" cloud save cloud-checkpoint '云端检查点'
)"
cloud_id="$(sed -n 's/^checkpoint_id=//p' <<< "$cloud_pending")"
grep -Fqx 'status=pending' <<< "$cloud_pending"
[[ "$cloud_id" == cloud-checkpoint ]]
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${source_env[@]}" "$tool" hook
cloud_list="$(env "${source_env[@]}" "$tool" cloud list)"
grep -Fq "$cloud_id" <<< "$cloud_list"
cloud_restore="$(env "${source_env[@]}" CODEX_HOME="$tmp/cloud-target/codex" \
    CLAUDE_CONFIG_DIR="$tmp/cloud-target/claude" "$tool" restore cloud "$cloud_id")"
grep -Fqx "saved_pwd=$tmp/work" <<< "$cloud_restore"
cmp "$tmp/source/codex/$session_rel" "$tmp/cloud-target/codex/$session_rel"
if env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
    "$tool" cloud save cloud-checkpoint 'Duplicate cloud ID' >/dev/null 2>&1; then
    echo 'cloud save accepted a duplicate ID' >&2
    exit 1
fi

queued_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
        "$tool" save active-window '活动窗口'
)"
queued_id="$(sed -n 's/^checkpoint_id=//p' <<< "$queued_output")"
queued_path="$(sed -n 's/^path=//p' <<< "$queued_output")"
grep -Fqx 'status=pending' <<< "$queued_output"
[[ "$queued_id" == active-window ]]
[[ ! -e "$queued_path" ]]
mkdir -p "$tmp/pending-set-target"
storage_before_pending_set="$(sha256sum "$tmp/state/storage.json")"
if env "${source_env[@]}" "$tool" set "$tmp/pending-set-target" \
    >"$tmp/pending-set.out" 2>"$tmp/pending-set.err"; then
    echo 'local storage changed while a checkpoint was pending' >&2
    exit 1
fi
grep -Fq 'cannot change local storage while a checkpoint or synchronization is pending' \
    "$tmp/pending-set.err"
[[ "$storage_before_pending_set" == "$(sha256sum "$tmp/state/storage.json")" ]]
printf '%s\n' '{"type":"assistant","complete":true}' >> "$tmp/source/codex/$session_rel"
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" "$tool" hook
[[ -f "$queued_path" ]]
[[ ! -d "$tmp/state/pending" ]] || ! find "$tmp/state/pending" -type f -name '*.json' | grep -q .

codex_active_home="$tmp/codex-active-target/codex"
mkdir -p "$codex_active_home/${session_rel%/*}"
cp -- "$tmp/source/codex/$session_rel" "$codex_active_home/$session_rel"
start_codex_session_process "$codex_active_home" "$session_id" "$session_rel"
codex_active_pid="$active_test_pid"
if env "${source_env[@]}" CODEX_HOME="$codex_active_home" \
    "$tool" resume "$queued_id" \
    >"$tmp/codex-active.out" 2>"$tmp/codex-active.err"; then
    echo 'resume accepted an active target Codex session' >&2
    exit 1
fi
grep -Fq \
    "codex session $session_id is already active in PID $codex_active_pid" \
    "$tmp/codex-active.err"
kill -0 "$codex_active_pid"
cmp "$tmp/source/codex/$session_rel" "$codex_active_home/$session_rel"
stop_test_process "$codex_active_pid"

codex_deleted_home="$tmp/codex-deleted-target/codex"
start_codex_session_process "$codex_deleted_home" "$session_id"
codex_deleted_pid="$active_test_pid"
codex_deleted_path="$active_codex_path"
rm -f -- "$codex_deleted_path"
codex_deleted_target="$(test_fd9_target "$codex_deleted_pid")"
[[ "$codex_deleted_target" == "$codex_deleted_path" ||
   "$codex_deleted_target" == "$codex_deleted_path (deleted)" ]]
if env "${source_env[@]}" CODEX_HOME="$codex_deleted_home" \
    "$tool" resume "$queued_id" \
    >"$tmp/codex-deleted.out" 2>"$tmp/codex-deleted.err"; then
    echo 'resume ignored an unlinked active Codex rollout' >&2
    exit 1
fi
grep -Fq \
    "codex session $session_id is already active in PID $codex_deleted_pid" \
    "$tmp/codex-deleted.err"
kill -0 "$codex_deleted_pid"
[[ ! -e "$codex_deleted_path" ]]
stop_test_process "$codex_deleted_pid"

codex_unrelated_id=77777777-8888-4999-8aaa-bbbbbbbbbbbb
codex_unrelated_home="$tmp/codex-unrelated-target/codex"
start_codex_session_process "$codex_unrelated_home" "$codex_unrelated_id"
write_codex_profile "$codex_unrelated_home" unrelated-running
codex_unrelated_pid="$active_test_pid"
codex_unrelated_resume="$(env "${source_env[@]}" CODEX_HOME="$codex_unrelated_home" \
    "$tool" resume "$queued_id" -- -punrelated-running)"
grep -Fqx \
    "FAKE_CODEX <resume> <$session_id> <-punrelated-running>" \
    <<< "$codex_unrelated_resume"
kill -0 "$codex_unrelated_pid"
cmp "$tmp/source/codex/$session_rel" "$codex_unrelated_home/$session_rel"
stop_test_process "$codex_unrelated_pid"

mkdir -p "$tmp/other-home/.local/bin"
printf '%s\n' '#!/usr/bin/env sh' 'printf '\''WRONG_CODEX\n'\''' \
    > "$tmp/other-home/.local/bin/codex"
if [[ "$test_platform" == Darwin ]]; then
    printf '%s\n' '#!/usr/bin/env sh' 'exec /usr/bin/pgrep "$@"' \
        > "$tmp/other-home/.local/bin/pgrep"
else
    printf '%s\n' '#!/usr/bin/env sh' 'exit 1' > "$tmp/other-home/.local/bin/pgrep"
fi
chmod +x "$tmp/other-home/.local/bin/codex" "$tmp/other-home/.local/bin/pgrep"
resume_output="$(env "${source_env[@]}" \
    PATH="$tmp/other-home/.local/bin:$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
    CODEX_HOME="$tmp/resume-target/codex" \
    "$tool" resume "$queued_id" --to codex -- \
        --to agent-owned --model test-model 'prompt words')"
grep -Fqx \
    "FAKE_CODEX <resume> <$session_id> <--to> <agent-owned> <--model> <test-model> <prompt words>" \
    <<< "$resume_output"
cmp "$tmp/source/codex/$session_rel" "$tmp/resume-target/codex/$session_rel"
if env "${source_env[@]}" CODEX_HOME="$tmp/resume-boundary-target/codex" \
    "$tool" resume "$queued_id" --model must-follow-separator --to claude \
    >"$tmp/resume-boundary.out" 2>"$tmp/resume-boundary.err"; then
    echo 'resume accepted an Agent argument before --' >&2
    exit 1
fi
grep -Fq 'unknown AGS resume argument: --model; put Agent arguments after --' \
    "$tmp/resume-boundary.err"
[[ ! -e "$tmp/resume-boundary-target" ]]

conversion_log="$tmp/home/.local/ags.log"
conversion_log_lines="$(wc -l < "$conversion_log")"
if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$tmp/version-gate-target/claude" \
    "$tool" resume "$queued_id" --to claude --force-unsupported-version \
    >"$tmp/version-gate.out" 2>"$tmp/version-gate.err"; then
    echo 'cross-Agent resume accepted a removed compatibility flag' >&2
    exit 1
fi
grep -Fq -- '--force-unsupported-version was removed' "$tmp/version-gate.err"
[[ "$conversion_log_lines" == "$(wc -l < "$conversion_log")" ]]
[[ ! -e "$tmp/version-gate-target" ]]

codex_to_claude_work="$tmp/codex-to-claude-work"
codex_to_claude_home="$tmp/codex-to-claude-target/claude"
codex_to_claude_settings="$codex_to_claude_home/sub2api.settings.json"
mkdir -p "$codex_to_claude_work" "$codex_to_claude_home"
printf '%s\n' \
    '{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example","ANTHROPIC_API_KEY":"","ANTHROPIC_AUTH_TOKEN":""},"apiKeyHelper":"/usr/bin/printenv SUB2API_API_KEY"}' \
    > "$codex_to_claude_settings"
codex_to_claude_settings_sha="$(
    sha256sum "$codex_to_claude_settings" | cut -d' ' -f1
)"
codex_to_claude_key="$(LC_ALL=C sed 's/[^A-Za-z0-9]/-/g' <<< "$codex_to_claude_work")"
codex_to_claude="$(env "${source_env[@]}" CLAUDE_CONFIG_DIR="$codex_to_claude_home" \
    OPENAI_API_KEY=must-not-leak ANTHROPIC_API_KEY=must-not-leak \
    "$tool" resume "$queued_id" --to claude \
        --profile sub2api --cwd "$codex_to_claude_work" -- \
        --model sonnet 'converted handoff')"
grep -Fqx \
    "FAKE_CLAUDE <--resume> <$converted_claude_id> <--settings> <$codex_to_claude_settings> <--model> <sonnet> <converted handoff>" \
    <<< "$codex_to_claude"
grep -Fqx "FAKE_PWD=$codex_to_claude_work" <<< "$codex_to_claude"
[[ "$codex_to_claude_settings_sha" == \
   "$(sha256sum "$codex_to_claude_settings" | cut -d' ' -f1)" ]]
grep -Fqx 'source_agent=codex' <<< "$codex_to_claude"
grep -Fqx 'agent=claude' <<< "$codex_to_claude"
grep -Fqx "source_session_id=$session_id" <<< "$codex_to_claude"
grep -Fqx "session_id=$converted_claude_id" <<< "$codex_to_claude"
grep -Fqx 'conversion=ags-0.3.0-test' <<< "$codex_to_claude"
codex_to_claude_file="$codex_to_claude_home/projects/$codex_to_claude_key/$converted_claude_id.jsonl"
[[ -f "$codex_to_claude_file" ]]
jq -s -e --arg cwd "$codex_to_claude_work" \
    'length > 0 and all(.[]; .cwd == $cwd)' \
    "$codex_to_claude_file" >/dev/null
grep -Fq 'converted reasoning summary' "$codex_to_claude_file"
! grep -Fq '"type":"thinking"' "$codex_to_claude_file"
[[ ! -e "$codex_to_claude_home/history.jsonl" ]]
grep -Eq "^HOME=$tmp/state/restore\\.[^/]+/ags/home$" "$conversion_log"
grep -Eq "^CODEX_HOME=$tmp/state/restore\\.[^/]+/ags/codex-home$" "$conversion_log"
grep -Eq "^CLAUDE_CONFIG_DIR=$tmp/state/restore\\.[^/]+/ags/claude-home$" \
    "$conversion_log"
grep -Fqx 'NO_STORE=1' "$conversion_log"

missing_claude_profile_home="$tmp/codex-to-missing-claude-profile-target/claude"
conversion_log_lines="$(wc -l < "$conversion_log")"
if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$missing_claude_profile_home" \
    "$tool" resume "$queued_id" --to claude \
        --profile missing \
    >"$tmp/missing-claude-profile.out" 2>"$tmp/missing-claude-profile.err"; then
    echo 'cross-Agent Claude resume accepted a missing profile' >&2
    exit 1
fi
grep -Fq 'Claude profile does not exist' "$tmp/missing-claude-profile.err"
[[ "$conversion_log_lines" == "$(wc -l < "$conversion_log")" ]]
[[ ! -e "$missing_claude_profile_home/projects" ]]

if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$codex_to_claude_home" \
    "$tool" resume "$queued_id" --to claude \
        --profile ../outside \
    >"$tmp/unsafe-claude-profile.out" 2>"$tmp/unsafe-claude-profile.err"; then
    echo 'Claude resume accepted an unsafe profile name' >&2
    exit 1
fi
grep -Fq 'invalid profile name' "$tmp/unsafe-claude-profile.err"

conversion_log_lines="$(wc -l < "$conversion_log")"
if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$codex_to_claude_home" \
    "$tool" resume "$queued_id" --to claude \
        --profile sub2api -- --settings "$tmp/other-claude-settings.json" \
    >"$tmp/conflicting-claude-settings.out" \
    2>"$tmp/conflicting-claude-settings.err"; then
    echo 'Claude resume accepted both --profile and --settings' >&2
    exit 1
fi
grep -Fq 'do not combine --profile with Claude --settings' \
    "$tmp/conflicting-claude-settings.err"
[[ "$conversion_log_lines" == "$(wc -l < "$conversion_log")" ]]

unicode_cross_work="$tmp/项目😀"
unicode_cross_home="$tmp/unicode-cross-target/claude"
mkdir -p "$unicode_cross_work"
unicode_parent_key="$(reference_ascii_claude_project_key "$tmp")"
unicode_cross_key="${unicode_parent_key}-----"
unicode_cross="$(env "${source_env[@]}" CLAUDE_CONFIG_DIR="$unicode_cross_home" \
    "$tool" resume "$queued_id" --to=claude \
        --cwd="$unicode_cross_work" -- -p 'print handoff')"
grep -Fqx \
    "FAKE_CLAUDE <--resume> <$converted_claude_id> <-p> <print handoff>" \
    <<< "$unicode_cross"
[[ -f "$unicode_cross_home/projects/$unicode_cross_key/$converted_claude_id.jsonl" ]]

rehome_id=12345678-1234-4234-8234-123456789abc
rehome_home="$tmp/rehome-target/claude"
rehome_source="$rehome_home/projects/original/$rehome_id.jsonl"
mkdir -p "${rehome_source%/*}"
printf '%s\n' '{"type":"user","source":"rehome"}' > "$rehome_source"
(
    cd "$unicode_cross_work"
    env "${source_env[@]}" CLAUDE_CONFIG_DIR="$rehome_home" \
        "$tool" rehome-claude "$rehome_id"
)
cmp "$rehome_source" "$rehome_home/projects/$unicode_cross_key/$rehome_id.jsonl"

long_cross_component="$(printf 'a%.0s' {1..210})"
long_cross_work="$tmp/$long_cross_component"
long_cross_home="$tmp/long-cross-target/claude"
mkdir -p "$long_cross_work"
long_cross_key="$(reference_ascii_claude_project_key "$long_cross_work")"
long_cross="$(env "${source_env[@]}" CLAUDE_CONFIG_DIR="$long_cross_home" \
    "$tool" resume "$queued_id" --to claude \
        --cwd "$long_cross_work")"
grep -Fqx "FAKE_CLAUDE <--resume> <$converted_claude_id>" <<< "$long_cross"
[[ -f "$long_cross_home/projects/$long_cross_key/$converted_claude_id.jsonl" ]]

mkdir -p "$tmp/vanishing-bin"
cp -- "$tmp/home/.local/bin/codex" "$tmp/vanishing-bin/codex"
vanishing_pending="$(
    cd "$tmp/work"
    env "${source_env[@]}" PATH="$tmp/vanishing-bin:$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin" \
        AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
        "$tool" save vanishing-binary 'Vanishing binary'
)"
grep -Fqx "agent_binary=$tmp/vanishing-bin/codex" <<< "$vanishing_pending"
rm -f -- "$tmp/vanishing-bin/codex"
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" "$tool" hook
vanishing_list="$(env "${source_env[@]}" "$tool" list)"
grep -Eq '^vanishing-binary +CODEX +' <<< "$vanishing_list"
vanishing_show="$(env "${source_env[@]}" "$tool" show vanishing-binary)"
grep -Eq "^Binary invoked +$tmp/vanishing-bin/codex$" <<< "$vanishing_show"
write_codex_profile "$tmp/vanishing-target/codex" fallback
vanishing_resume="$(env "${source_env[@]}" CODEX_HOME="$tmp/vanishing-target/codex" \
    "$tool" resume vanishing-binary --profile fallback 2>"$tmp/vanishing.err")"
grep -Fqx "FAKE_CODEX <resume> <$session_id> <--profile> <fallback>" <<< "$vanishing_resume"
grep -Fq "saved codex binary is unavailable: $tmp/vanishing-bin/codex" "$tmp/vanishing.err"

claude_session_id=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee
claude_initial_key="$(LC_ALL=C sed 's/[^A-Za-z0-9]/-/g' <<< "$tmp/work")"
claude_initial_rel="projects/$claude_initial_key/$claude_session_id.jsonl"
mkdir -p "$tmp/source/claude/$(dirname "$claude_initial_rel")"
printf '%s\n' '{"type":"user","source":"initial"}' \
    > "$tmp/source/claude/$claude_initial_rel"
claude_pending="$(
    cd "$tmp/work"
    env "${source_env[@]}" CLAUDE_CODE_SESSION_ID="$claude_session_id" \
        "$tool" save claude-window 'Claude 恢复检查'
)"
grep -Fqx 'checkpoint_id=claude-window' <<< "$claude_pending"
grep -Fqx 'agent=claude' <<< "$claude_pending"
grep -Fqx "agent_binary=$tmp/home/.local/bin/claude" <<< "$claude_pending"
claude_path="$(sed -n 's/^path=//p' <<< "$claude_pending")"

claude_hook_work="$tmp/claude-hook-work"
claude_hook_key=hook-project
claude_session_rel="projects/$claude_hook_key/$claude_session_id.jsonl"
claude_session_tree="projects/$claude_hook_key/$claude_session_id"
mkdir -p "$claude_hook_work" \
    "$tmp/source/claude/$claude_session_tree/subagents" \
    "$tmp/source/claude/$claude_session_tree/tool-results" \
    "$tmp/source/claude/file-history/$claude_session_id" \
    "$tmp/source/claude/tasks/$claude_session_id" \
    "$tmp/source/claude/session-env/$claude_session_id"
printf '%s\n' '{"type":"user","source":"hook"}' '{"type":"assistant"}' \
    > "$tmp/source/claude/$claude_session_rel"
printf '%s\n' '{"agent":"child"}' \
    > "$tmp/source/claude/$claude_session_tree/subagents/child.jsonl"
printf '%s\n' 'tool result' \
    > "$tmp/source/claude/$claude_session_tree/tool-results/result.txt"
printf '%s\n' 'history' \
    > "$tmp/source/claude/file-history/$claude_session_id/history.txt"
printf '%s\n' '{"task":"resume"}' \
    > "$tmp/source/claude/tasks/$claude_session_id/task.json"
printf '%s\n' 'SESSION_ENV=test' \
    > "$tmp/source/claude/session-env/$claude_session_id/env"
claude_relatives=(
    "$claude_session_rel"
    "$claude_session_tree/subagents/child.jsonl"
    "$claude_session_tree/tool-results/result.txt"
    "file-history/$claude_session_id/history.txt"
    "tasks/$claude_session_id/task.json"
    "session-env/$claude_session_id/env"
)
claude_modes=(640 600 444 640 600 600)
claude_mtimes=(1700000101 1700000102 1700000103 1700000104 1700000105 1700000106)
for metadata_index in "${!claude_relatives[@]}"; do
    chmod "${claude_modes[$metadata_index]}" \
        "$tmp/source/claude/${claude_relatives[$metadata_index]}"
    touch -d "@${claude_mtimes[$metadata_index]}" -- \
        "$tmp/source/claude/${claude_relatives[$metadata_index]}"
done
printf '{"hook_event_name":"Stop","session_id":"%s","transcript_path":"%s","cwd":"%s"}\n' \
    "$claude_session_id" "$tmp/source/claude/$claude_session_rel" "$claude_hook_work" | \
    env "${source_env[@]}" AGENT_SESSION_LOCAL_DIR="$tmp/local-checkpoints" "$tool" hook
[[ -f "$claude_path" ]]
assert_format4_archive "$claude_path" "$tmp/extracted/claude"
grep -Fqx 'agent=claude' "$tmp/extracted/claude/manifest"
grep -Fqx "relative_path=$claude_session_rel" "$tmp/extracted/claude/manifest"
grep -Fqx "cwd=$claude_hook_work" "$tmp/extracted/claude/manifest"
grep -Fqx 'artifact_count=6' "$tmp/extracted/claude/manifest"
for metadata_index in "${!claude_relatives[@]}"; do
    relative="${claude_relatives[$metadata_index]}"
    cmp "$tmp/source/claude/$relative" "$tmp/extracted/claude/artifacts/$relative"
    grep -Eq \
        "^[0-9a-f]{64}"$'\t'"[0-9]+"$'\t'"$relative"$'\t'"${claude_modes[$metadata_index]}"$'\t'"${claude_mtimes[$metadata_index]}$" \
        "$tmp/extracted/claude/artifacts.tsv"
done

claude_active_home="$tmp/claude-active-target/claude"
mkdir -p "$claude_active_home/${claude_session_rel%/*}"
cp -- "$tmp/source/claude/$claude_session_rel" \
    "$claude_active_home/$claude_session_rel"
start_claude_session_process "$claude_active_home" "$claude_session_id"
claude_active_pid="$active_test_pid"
if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$claude_active_home" \
    "$tool" resume claude-window \
    >"$tmp/claude-active.out" 2>"$tmp/claude-active.err"; then
    echo 'resume accepted an active target Claude session' >&2
    exit 1
fi
grep -Fq \
    "claude session $claude_session_id is already active in PID $claude_active_pid" \
    "$tmp/claude-active.err"
kill -0 "$claude_active_pid"
cmp "$tmp/source/claude/$claude_session_rel" \
    "$claude_active_home/$claude_session_rel"
stop_test_process "$claude_active_pid"

claude_unrelated_id=cccccccc-dddd-4eee-8fff-111111111111
claude_unrelated_home="$tmp/claude-unrelated-target/claude"
start_claude_session_process "$claude_unrelated_home" "$claude_unrelated_id"
claude_unrelated_pid="$active_test_pid"
claude_unrelated_resume="$(
    env "${source_env[@]}" CLAUDE_CONFIG_DIR="$claude_unrelated_home" \
        "$tool" resume claude-window -- -p 'print native'
)"
grep -Fqx \
    "FAKE_CLAUDE <--resume> <$claude_session_id> <-p> <print native>" \
    <<< "$claude_unrelated_resume"
kill -0 "$claude_unrelated_pid"
stop_test_process "$claude_unrelated_pid"

claude_stale_home="$tmp/claude-stale-target/claude"
start_claude_session_process "$claude_stale_home" "$claude_session_id" 1
claude_stale_pid="$active_test_pid"
claude_stale_resume="$(
    env "${source_env[@]}" CLAUDE_CONFIG_DIR="$claude_stale_home" \
        "$tool" resume claude-window -- --model stale-registry
)"
grep -Fqx \
    "FAKE_CLAUDE <--resume> <$claude_session_id> <--model> <stale-registry>" \
    <<< "$claude_stale_resume"
kill -0 "$claude_stale_pid"
stop_test_process "$claude_stale_pid"

rollback_home="$tmp/rollback-target/claude"
rollback_main="$rollback_home/$claude_session_rel"
mkdir -p "$(dirname "$rollback_main")"
head -n 1 "$tmp/source/claude/$claude_session_rel" > "$rollback_main"
chmod 604 "$rollback_main"
touch -d '@1600000000' -- "$rollback_main"
cp -p "$rollback_main" "$tmp/rollback-before.jsonl"
if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$rollback_home" \
    AGENT_SESSION_TEST_FAIL_REPLACE_AT=2 \
    "$tool" restore local claude-window \
    >"$tmp/rollback.out" 2>"$tmp/rollback.err"; then
    echo 'restore ignored an injected multi-artifact write failure' >&2
    exit 1
fi
grep -Fq 'rolling back 1 committed artifact(s)' "$tmp/rollback.err"
grep -Fq 'rolled back original artifact' "$tmp/rollback.err"
grep -Fq 'restore rollback complete; no artifact changes were retained' "$tmp/rollback.err"
cmp "$tmp/rollback-before.jsonl" "$rollback_main"
[[ "$(stat -c '%a:%Y' "$rollback_main")" == 604:1600000000 ]]
[[ "$(find "$rollback_home" -type f | wc -l)" == 1 ]]

symlink_home="$tmp/symlink-escape-target/claude"
symlink_outside="$tmp/symlink-escape-outside"
mkdir -p "$symlink_home/projects/$claude_hook_key" "$symlink_outside"
ln -s "$symlink_outside" \
    "$symlink_home/projects/$claude_hook_key/$claude_session_id"
if env "${source_env[@]}" CLAUDE_CONFIG_DIR="$symlink_home" \
    "$tool" restore local claude-window \
    >"$tmp/symlink-escape.out" 2>"$tmp/symlink-escape.err"; then
    echo 'restore followed a symbolic-link artifact ancestor' >&2
    exit 1
fi
grep -Fq 'restore destination has a symbolic-link ancestor' "$tmp/symlink-escape.err"
[[ ! -e "$symlink_home/$claude_session_rel" ]]
[[ -z "$(find "$symlink_outside" -mindepth 1 -print -quit)" ]]

claude_resume_work="$tmp/claude-resume-work"
mkdir -p "$claude_resume_work"
claude_resume_key="$(LC_ALL=C sed 's/[^A-Za-z0-9]/-/g' <<< "$claude_resume_work")"
claude_resume_rel="projects/$claude_resume_key/$claude_session_id.jsonl"
claude_resume_tree="projects/$claude_resume_key/$claude_session_id"
claude_resume="$(env "${source_env[@]}" CLAUDE_CONFIG_DIR="$tmp/claude-resume-target/claude" \
    "$tool" resume claude-window --cwd "$claude_resume_work" -- \
        --model sonnet 'continue here')"
grep -Fqx "FAKE_CLAUDE <--resume> <$claude_session_id> <--model> <sonnet> <continue here>" <<< "$claude_resume"
grep -Fqx "FAKE_PWD=$claude_resume_work" <<< "$claude_resume"
[[ "$claude_resume" != *'<-->'* ]]
cmp "$tmp/source/claude/$claude_session_rel" \
    "$tmp/claude-resume-target/claude/$claude_resume_rel"
cmp "$tmp/source/claude/$claude_session_tree/subagents/child.jsonl" \
    "$tmp/claude-resume-target/claude/$claude_resume_tree/subagents/child.jsonl"
cmp "$tmp/source/claude/$claude_session_tree/tool-results/result.txt" \
    "$tmp/claude-resume-target/claude/$claude_resume_tree/tool-results/result.txt"
for relative in \
    "file-history/$claude_session_id/history.txt" \
    "tasks/$claude_session_id/task.json" \
    "session-env/$claude_session_id/env"; do
    cmp "$tmp/source/claude/$relative" "$tmp/claude-resume-target/claude/$relative"
done
[[ ! -e "$tmp/claude-resume-target/claude/$claude_session_rel" ]]
claude_restored_relatives=(
    "$claude_resume_rel"
    "$claude_resume_tree/subagents/child.jsonl"
    "$claude_resume_tree/tool-results/result.txt"
    "file-history/$claude_session_id/history.txt"
    "tasks/$claude_session_id/task.json"
    "session-env/$claude_session_id/env"
)
for metadata_index in "${!claude_relatives[@]}"; do
    [[ "$(stat -c '%a:%Y' \
        "$tmp/claude-resume-target/claude/${claude_restored_relatives[$metadata_index]}")" == \
       "${claude_modes[$metadata_index]}:${claude_mtimes[$metadata_index]}" ]]
done

claude_profile_home="$tmp/claude-profile-target/claude"
claude_profile_settings="$claude_profile_home/sub2api.settings.json"
mkdir -p "$claude_profile_home"
printf '%s\n' \
    '{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example","ANTHROPIC_API_KEY":"","ANTHROPIC_AUTH_TOKEN":""},"apiKeyHelper":"/usr/bin/printenv SUB2API_API_KEY"}' \
    > "$claude_profile_settings"
claude_profile_sha="$(sha256sum "$claude_profile_settings" | cut -d' ' -f1)"
claude_profile_resume="$(
    env "${source_env[@]}" CLAUDE_CONFIG_DIR="$claude_profile_home" \
        "$tool" resume claude-window --profile sub2api
)"
grep -Fqx \
    "FAKE_CLAUDE <--resume> <$claude_session_id> <--settings> <$claude_profile_settings>" \
    <<< "$claude_profile_resume"
[[ "$claude_profile_sha" == "$(sha256sum "$claude_profile_settings" | cut -d' ' -f1)" ]]

claude_to_codex_home="$tmp/claude-to-codex-target/codex"
mkdir -p "$claude_to_codex_home"
printf '%s\n' 'model_provider = "sub2api"' \
    > "$claude_to_codex_home/sub2api.config.toml"
profile_after_separator_home="$tmp/profile-after-separator/codex"
mkdir -p "$profile_after_separator_home"
printf '%s\n' 'model_provider = "sub2api"' \
    > "$profile_after_separator_home/sub2api.config.toml"
profile_after_separator="$(
    env "${source_env[@]}" CODEX_HOME="$profile_after_separator_home" \
    "$tool" resume claude-window --to codex -- \
        -p sub2api
)"
grep -Fqx \
    "FAKE_CODEX <resume> <$converted_codex_id> <-p> <sub2api>" \
    <<< "$profile_after_separator"
grep -Fqx 'source_agent=claude' <<< "$profile_after_separator"
grep -Fqx 'agent=codex' <<< "$profile_after_separator"
profile_after_separator_file="$profile_after_separator_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl"
jq -s -e '
    [.[] | select(.type == "session_meta")] |
    length == 1 and
    all(.[]; .payload.model_provider == "sub2api")
' "$profile_after_separator_file" >/dev/null

profile_long_home="$tmp/profile-long/codex"
write_codex_profile "$profile_long_home" sub2api sub2api
profile_long="$(
    env "${source_env[@]}" CODEX_HOME="$profile_long_home" \
    "$tool" resume claude-window --to codex -- \
        --profile sub2api
)"
grep -Fqx \
    "FAKE_CODEX <resume> <$converted_codex_id> <--profile> <sub2api>" \
    <<< "$profile_long"
jq -s -e '
    [.[] | select(.type == "session_meta")] |
    length == 1 and
    all(.[]; .payload.model_provider == "sub2api")
' "$profile_long_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl" \
    >/dev/null

profile_equals_home="$tmp/profile-equals/codex"
write_codex_profile "$profile_equals_home" sub2api sub2api
profile_equals="$(
    env "${source_env[@]}" CODEX_HOME="$profile_equals_home" \
    "$tool" resume claude-window --to codex -- \
        --profile=sub2api
)"
grep -Fqx \
    "FAKE_CODEX <resume> <$converted_codex_id> <--profile=sub2api>" \
    <<< "$profile_equals"
jq -s -e '
    [.[] | select(.type == "session_meta")] |
    length == 1 and
    all(.[]; .payload.model_provider == "sub2api")
' "$profile_equals_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl" \
    >/dev/null

duplicate_profile_home="$tmp/profile-duplicate/codex"
write_codex_profile "$duplicate_profile_home" sub2api sub2api
if env "${source_env[@]}" CODEX_HOME="$duplicate_profile_home" \
    "$tool" resume claude-window --to codex \
        --profile sub2api -- --profile=sub2api \
        >"$tmp/profile-duplicate.out" 2>"$tmp/profile-duplicate.err"; then
    echo 'resume accepted both AGS and native Codex profiles' >&2
    exit 1
fi
grep -Fq 'do not combine AGS --profile with Codex -p/--profile' \
    "$tmp/profile-duplicate.err"
[[ ! -e "$duplicate_profile_home/sessions" ]]

assert_cross_codex_provider_override_rejected() {
    local label="$1" target_home before
    shift
    target_home="$tmp/codex-provider-override-$label/codex"
    before="$(wc -l < "$conversion_log")"
    if env "${source_env[@]}" CODEX_HOME="$target_home" \
        "$tool" resume claude-window --to codex -- "$@" \
        >"$tmp/codex-provider-override-$label.out" \
        2>"$tmp/codex-provider-override-$label.err"; then
        echo "cross-Agent Codex resume accepted provider override: $label" >&2
        exit 1
    fi
    case "$label" in
        config|provider-config)
            grep -Fq 'cannot be forwarded because it changes the workspace or configuration after AGS validated it' \
                "$tmp/codex-provider-override-$label.err"
            ;;
        *)
            grep -Fq 'do not override the Codex provider' \
                "$tmp/codex-provider-override-$label.err"
            ;;
    esac
    [[ "$before" == "$(wc -l < "$conversion_log")" ]]
    [[ ! -e "$target_home/sessions" ]]
}

assert_cross_codex_provider_override_rejected \
    config -c 'model_provider="other"'
assert_cross_codex_provider_override_rejected \
    provider-config --config=model_providers.sub2api.base_url='"https://other.example"'
assert_cross_codex_provider_override_rejected oss --oss
assert_cross_codex_provider_override_rejected \
    local-provider --local-provider ollama

cross_unrelated_codex_id=22222222-3333-4444-8555-666666666666
start_codex_session_process "$claude_to_codex_home" "$cross_unrelated_codex_id"
cross_unrelated_codex_pid="$active_test_pid"
cross_unrelated_codex_path="$active_codex_path"
claude_to_codex="$(env "${source_env[@]}" CODEX_HOME="$claude_to_codex_home" \
    OPENAI_API_KEY=must-not-leak ANTHROPIC_API_KEY=must-not-leak \
    "$tool" resume claude-window --to codex \
        --profile sub2api)"
grep -Fqx \
    "FAKE_CODEX <resume> <$converted_codex_id> <--profile> <sub2api>" \
    <<< "$claude_to_codex"
grep -Fqx 'source_agent=claude' <<< "$claude_to_codex"
grep -Fqx 'agent=codex' <<< "$claude_to_codex"
grep -Fqx "source_session_id=$claude_session_id" <<< "$claude_to_codex"
grep -Fqx "session_id=$converted_codex_id" <<< "$claude_to_codex"
claude_to_codex_file="$claude_to_codex_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl"
[[ -f "$claude_to_codex_file" ]]
jq -s -e --arg cwd "$claude_hook_work" '
    ([.[] | select(.type == "session_meta")] |
        length == 1 and
        all(.[]; .payload.model_provider == "sub2api" and .payload.cwd == $cwd)) and
    ([.[] | select(
        .type == "turn_context" and
        ((.payload.workspace_roots? | type) == "array")
    )] |
        length > 0 and all(.[]; .payload.workspace_roots == [$cwd]))
' "$claude_to_codex_file" >/dev/null || {
    echo 'converted Codex transcript ignored the selected provider or target cwd' >&2
    exit 1
}
grep -Fqx \
    "REGISTER=$converted_codex_id"$'\t'"$claude_to_codex_file"$'\t'"$claude_hook_work" \
    "$conversion_log"
[[ ! -e "$claude_to_codex_home/session_index.jsonl" ]]
kill -0 "$cross_unrelated_codex_pid"
stop_test_process "$cross_unrelated_codex_pid"
rm -f -- "$cross_unrelated_codex_path"
[[ "$(find "$claude_to_codex_home/sessions" -type f | wc -l)" == 1 ]]

register_failure_home="$tmp/register-failure-target/codex"
register_failure_file="$register_failure_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl"
if env "${source_env[@]}" CODEX_HOME="$register_failure_home" \
    AGS_REGISTER_FAIL=1 \
    "$tool" resume claude-window --to codex --cwd "$claude_hook_work" \
    >"$tmp/register-failure.out" 2>"$tmp/register-failure.err"; then
    echo 'Codex resume ignored a failed thread-index registration' >&2
    exit 1
fi
grep -Fq 'cannot register restored Codex session in its thread index' \
    "$tmp/register-failure.err"
grep -Fq 'restore rollback complete; no artifact changes were retained' \
    "$tmp/register-failure.err"
grep -Fqx \
    "REGISTER=$converted_codex_id"$'\t'"$register_failure_file"$'\t'"$claude_hook_work" \
    "$conversion_log"
[[ ! -e "$register_failure_file" ]]
[[ ! -d "$register_failure_home/sessions" ]]

claude_to_default_codex_home="$tmp/claude-to-default-codex-target/codex"
claude_to_default_codex="$(env "${source_env[@]}" \
    CODEX_HOME="$claude_to_default_codex_home" \
    "$tool" resume claude-window --to codex)"
grep -Fqx "FAKE_CODEX <resume> <$converted_codex_id>" \
    <<< "$claude_to_default_codex"
claude_to_default_codex_file="$claude_to_default_codex_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl"
jq -e '
    select(.type == "session_meta") |
    .payload.model_provider == "openai"
' "$claude_to_default_codex_file" >/dev/null || {
    echo 'converted Codex transcript did not use the default provider' >&2
    exit 1
}

claude_to_base_codex_home="$tmp/claude-to-base-codex-target/codex"
mkdir -p "$claude_to_base_codex_home"
printf '%s\n' "model_provider = 'basegateway'" \
    > "$claude_to_base_codex_home/config.toml"
claude_to_base_codex="$(env "${source_env[@]}" \
    CODEX_HOME="$claude_to_base_codex_home" \
    "$tool" resume claude-window --to codex)"
grep -Fqx "FAKE_CODEX <resume> <$converted_codex_id>" \
    <<< "$claude_to_base_codex"
claude_to_base_codex_file="$claude_to_base_codex_home/sessions/2026/07/25/rollout-test-$converted_codex_id.jsonl"
jq -e '
    select(.type == "session_meta") |
    .payload.model_provider == "basegateway"
' "$claude_to_base_codex_file" >/dev/null || {
    echo 'converted Codex transcript ignored the base config provider' >&2
    exit 1
}

missing_profile_home="$tmp/claude-to-missing-profile-target/codex"
conversion_log_lines="$(wc -l < "$conversion_log")"
if env "${source_env[@]}" CODEX_HOME="$missing_profile_home" \
    "$tool" resume claude-window --to codex \
        --profile missing \
    >"$tmp/missing-profile.out" 2>"$tmp/missing-profile.err"; then
    echo 'cross-Agent Codex resume accepted a missing profile' >&2
    exit 1
fi
grep -Fq 'Codex profile does not exist' "$tmp/missing-profile.err"
[[ "$conversion_log_lines" == "$(wc -l < "$conversion_log")" ]]
[[ ! -d "$missing_profile_home/sessions" ]]

cat > "$tmp/home/.local/bin/casr-fail" <<'EOF'
#!/usr/bin/env sh
if [ "${1:-}" = --version ]; then printf 'casr 0.3.0-test\n'; exit; fi
if [ "${1:-}" = terminal-attach ]; then exit 3; fi
printf '{"message":"synthetic conversion failure"}\n' >&2
exit 42
EOF
chmod +x "$tmp/home/.local/bin/casr-fail"
if env "${source_env[@]}" CODEX_HOME="$tmp/conversion-failure-target/codex" \
    AGS_CONVERTER_BINARY="$tmp/home/.local/bin/casr-fail" \
    "$tool" resume claude-window --to codex \
    >"$tmp/conversion-failure.out" 2>"$tmp/conversion-failure.err"; then
    echo 'resume ignored an ags conversion failure' >&2
    exit 1
fi
grep -Fq 'ags conversion failed: synthetic conversion failure' "$tmp/conversion-failure.err"
[[ ! -e "$tmp/conversion-failure-target/codex/sessions" ]]

legacy_id='Legacy_描述--20260101T000000.000000000Z'
legacy_session_id=99999999-8888-4777-8666-555555555555
legacy_relative="sessions/2026/01/01/legacy-$legacy_session_id.jsonl"
legacy_payload="$tmp/legacy/payload"
mkdir -p "$legacy_payload" "$tmp/local-checkpoints/codex"
printf '%s\n' '{"type":"legacy"}' > "$legacy_payload/session.jsonl"
legacy_checksum="$(sha256sum "$legacy_payload/session.jsonl" | cut -d' ' -f1)"
{
    printf 'format=2\nkind=checkpoint\ncheckpoint_id=%s\n' "$legacy_id"
    printf 'description=旧版检查点\ncreated_utc=2026-01-01T00:00:00.000Z\ntarget=local\n'
    printf 'agent=codex\nsession_id=%s\nrelative_path=%s\n' \
        "$legacy_session_id" "$legacy_relative"
    printf 'cwd=%s\nsource_size=%s\nsource_sha256=%s\n' \
        "$tmp/work" "$(stat -c %s "$legacy_payload/session.jsonl")" "$legacy_checksum"
    printf 'public_ip=unknown\nip_location=unknown\ngeo_provider=unknown\n'
} > "$legacy_payload/manifest"
ssh-keygen -y -f "$tmp/key" > "$tmp/legacy-recipient.pub"
tar -C "$legacy_payload" -czf - manifest session.jsonl | \
    age -R "$tmp/legacy-recipient.pub" \
        -o "$tmp/local-checkpoints/codex/$legacy_id.checkpoint.tar.gz.age"
legacy_list="$(env "${source_env[@]}" "$tool" list)"
grep -Fq "$legacy_id" <<< "$legacy_list"
grep -Fq '旧版检查点' <<< "$legacy_list"
grep -Eq '^sat-index +CODEX +' <<< "$legacy_list"
grep -Eq '^claude-window +CLAUDE +' <<< "$legacy_list"
legacy_resume="$(env "${source_env[@]}" CODEX_HOME="$tmp/legacy-resume-target/codex" \
    "$tool" resume "$legacy_id" -- --sandbox read-only)"
grep -Fqx "FAKE_CODEX <resume> <$legacy_session_id> <--sandbox> <read-only>" <<< "$legacy_resume"
cmp "$legacy_payload/session.jsonl" "$tmp/legacy-resume-target/codex/$legacy_relative"

collision_id=collision
collision_record_id=abcdefabcdefabcdefabcdef@collision
{
    printf 'format=2\nkind=checkpoint\ncheckpoint_id=%s\n' "$collision_id"
    printf 'description=Legacy collision\ncreated_utc=2026-01-01T00:00:00.000Z\ntarget=local\n'
    printf 'agent=codex\nsession_id=%s\nrelative_path=%s\n' \
        "$legacy_session_id" "$legacy_relative"
    printf 'cwd=%s\nsource_size=%s\nsource_sha256=%s\n' \
        "$tmp/work" "$(stat -c %s "$legacy_payload/session.jsonl")" "$legacy_checksum"
    printf 'public_ip=unknown\nip_location=unknown\ngeo_provider=unknown\n'
} > "$legacy_payload/manifest"
tar -C "$legacy_payload" -czf - manifest session.jsonl | \
    age -R "$tmp/legacy-recipient.pub" \
        -o "$tmp/local-checkpoints/codex/$collision_id.checkpoint.tar.gz.age"
{
    printf 'format=3\nkind=checkpoint\ncheckpoint_id=%s\nrecord_id=%s\n' \
        "$collision_id" "$collision_record_id"
    printf 'description=New collision\ncreated_utc=2026-01-02T00:00:00.000Z\ntarget=local\n'
    printf 'agent=codex\nagent_binary=%s\nsession_id=%s\nrelative_path=%s\n' \
        "$tmp/home/.local/bin/codex" "$legacy_session_id" "$legacy_relative"
    printf 'cwd=%s\nsource_size=%s\nsource_sha256=%s\n' \
        "$tmp/work" "$(stat -c %s "$legacy_payload/session.jsonl")" "$legacy_checksum"
    printf 'public_ip=unknown\nip_location=unknown\ngeo_provider=unknown\n'
} > "$legacy_payload/manifest"
tar -C "$legacy_payload" -czf - manifest session.jsonl | \
    age -R "$tmp/legacy-recipient.pub" \
        -o "$tmp/local-checkpoints/codex/$collision_record_id.checkpoint.tar.gz.age"
[[ "$(env "${source_env[@]}" "$tool" list | grep -Ec '^collision +CODEX +')" == 2 ]]
if env "${source_env[@]}" "$tool" resume "$collision_id" \
    >"$tmp/collision.out" 2>"$tmp/collision.err"; then
    echo 'resume allowed a format 2/3 logical-ID collision' >&2
    exit 1
fi
grep -Fq 'matching RECORD_ID values:' "$tmp/collision.err"
grep -Fq "codex/$collision_id" "$tmp/collision.err"
grep -Fq "codex/$collision_record_id" "$tmp/collision.err"
grep -Fq 'expected one checkpoint named collision; found 2' "$tmp/collision.err"
legacy_collision_show="$(env "${source_env[@]}" "$tool" show "codex/$collision_id")"
grep -Eq "^Selector +codex/$collision_id$" <<< "$legacy_collision_show"
grep -Eq '^Format +2$' <<< "$legacy_collision_show"
grep -Eq '^Description +Legacy collision$' <<< "$legacy_collision_show"
new_collision_show="$(env "${source_env[@]}" "$tool" show "codex/$collision_record_id")"
grep -Eq "^Selector +codex/$collision_record_id$" <<< "$new_collision_show"
grep -Eq '^Format +3$' <<< "$new_collision_show"
grep -Eq '^Description +New collision$' <<< "$new_collision_show"
write_codex_profile "$tmp/collision-legacy-target/codex" legacy-collision
legacy_collision_resume="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/collision-legacy-target/codex" \
        "$tool" resume "codex/$collision_id" --profile legacy-collision
)"
grep -Fqx \
    "FAKE_CODEX <resume> <$legacy_session_id> <--profile> <legacy-collision>" \
    <<< "$legacy_collision_resume"
write_codex_profile "$tmp/collision-new-target/codex" new-collision
new_collision_resume="$(
    env "${source_env[@]}" CODEX_HOME="$tmp/collision-new-target/codex" \
        "$tool" resume "codex/$collision_record_id" --profile new-collision
)"
grep -Fqx \
    "FAKE_CODEX <resume> <$legacy_session_id> <--profile> <new-collision>" \
    <<< "$new_collision_resume"
legacy_collision_delete="$(env "${source_env[@]}" "$tool" delete "codex/$collision_id")"
grep -Fqx "record_id=$collision_id" <<< "$legacy_collision_delete"
[[ ! -e "$tmp/local-checkpoints/codex/$collision_id.checkpoint.tar.gz.age" ]]
new_collision_delete="$(env "${source_env[@]}" "$tool" delete "codex/$collision_record_id")"
grep -Fqx "record_id=$collision_record_id" <<< "$new_collision_delete"
[[ ! -e "$tmp/local-checkpoints/codex/$collision_record_id.checkpoint.tar.gz.age" ]]

unsafe_id=unsafe-binary
unsafe_record_id=0123456789abcdef01234567@unsafe-binary
{
    printf 'format=3\nkind=checkpoint\ncheckpoint_id=%s\nrecord_id=%s\n' \
        "$unsafe_id" "$unsafe_record_id"
    printf 'description=Unsafe binary\ncreated_utc=2026-01-01T00:00:00.000Z\ntarget=local\n'
    printf 'agent=codex\nagent_binary=/bin/sh\nsession_id=%s\nrelative_path=%s\n' \
        "$legacy_session_id" "$legacy_relative"
    printf 'cwd=%s\nsource_size=%s\nsource_sha256=%s\n' \
        "$tmp/work" "$(stat -c %s "$legacy_payload/session.jsonl")" "$legacy_checksum"
    printf 'public_ip=unknown\nip_location=unknown\ngeo_provider=unknown\n'
} > "$legacy_payload/manifest"
tar -C "$legacy_payload" -czf - manifest session.jsonl | \
    age -R "$tmp/legacy-recipient.pub" \
        -o "$tmp/local-checkpoints/codex/$unsafe_record_id.checkpoint.tar.gz.age"
if env "${source_env[@]}" CODEX_HOME="$tmp/unsafe-target/codex" \
    "$tool" restore local "$unsafe_id" >/dev/null 2>&1; then
    echo 'restore accepted an agent binary for the wrong executable' >&2
    exit 1
fi
[[ ! -e "$tmp/unsafe-target/codex/$legacy_relative" ]]

mkdir -p "$tmp/archive-controlled"
printf '%s\n' '#!/usr/bin/env sh' 'printf '\''MALICIOUS_ARCHIVE_BINARY\n'\''' \
    > "$tmp/archive-controlled/codex"
chmod +x "$tmp/archive-controlled/codex"
archive_binary_id=archive-binary
archive_binary_record_id=1234567890abcdef12345678@archive-binary
{
    printf 'format=3\nkind=checkpoint\ncheckpoint_id=%s\nrecord_id=%s\n' \
        "$archive_binary_id" "$archive_binary_record_id"
    printf 'description=Archive binary path\ncreated_utc=2026-01-01T00:00:00.000Z\ntarget=local\n'
    printf 'agent=codex\nagent_binary=%s\nsession_id=%s\nrelative_path=%s\n' \
        "$tmp/archive-controlled/codex" "$legacy_session_id" "$legacy_relative"
    printf 'cwd=%s\nsource_size=%s\nsource_sha256=%s\n' \
        "$tmp/work" "$(stat -c %s "$legacy_payload/session.jsonl")" "$legacy_checksum"
    printf 'public_ip=unknown\nip_location=unknown\ngeo_provider=unknown\n'
} > "$legacy_payload/manifest"
tar -C "$legacy_payload" -czf - manifest session.jsonl | \
    age -R "$tmp/legacy-recipient.pub" \
        -o "$tmp/local-checkpoints/codex/$archive_binary_record_id.checkpoint.tar.gz.age"
write_codex_profile "$tmp/archive-binary-target/codex" trusted
archive_binary_resume="$(env "${source_env[@]}" CODEX_HOME="$tmp/archive-binary-target/codex" \
    "$tool" resume "$archive_binary_id" -- --profile trusted)"
grep -Fqx \
    "FAKE_CODEX <resume> <$legacy_session_id> <--profile> <trusted>" \
    <<< "$archive_binary_resume"
[[ "$archive_binary_resume" != *MALICIOUS_ARCHIVE_BINARY* ]]

if env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
    "$tool" save missing-description >/dev/null 2>&1; then
    echo 'save accepted a missing description' >&2
    exit 1
fi
if env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
    "$tool" save 'bad id' 'Invalid ID' >/dev/null 2>&1; then
    echo 'save accepted an invalid ID' >&2
    exit 1
fi
if env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
    "$tool" save active-window 'Duplicate ID' >/dev/null 2>&1; then
    echo 'save accepted a duplicate ID' >&2
    exit 1
fi

startup_output="$(
    cd "$tmp/work"
    env "${source_env[@]}" AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
        "$tool" save startup-recovery '意外关闭恢复'
)"
startup_id="$(sed -n 's/^checkpoint_id=//p' <<< "$startup_output")"
startup_path="$(sed -n 's/^path=//p' <<< "$startup_output")"
[[ ! -e "$startup_path" ]]
printf '%s\n' '{"hook_event_name":"SessionStart","session_id":"99999999-9999-4999-8999-999999999999"}' | \
    env "${source_env[@]}" "$tool" hook
[[ -f "$startup_path" ]]
env "${source_env[@]}" "$tool" list | grep -Fq "$startup_id"
delete_output="$(env "${source_env[@]}" "$tool" delete "$startup_id")"
grep -Fqx 'status=deleted' <<< "$delete_output"
recoverable_path="$(sed -n 's/^recoverable_path=//p' <<< "$delete_output")"
[[ -f "$recoverable_path" && ! -e "$startup_path" ]]
! env "${source_env[@]}" "$tool" list | grep -Fq "$startup_id"

cloud_delete="$(env "${source_env[@]}" "$tool" cloud delete "$cloud_id")"
grep -Fqx 'status=deleted' <<< "$cloud_delete"
grep -Fqx 'target=cloud' <<< "$cloud_delete"
! env "${source_env[@]}" "$tool" cloud list | grep -Fq "$cloud_id"

password_config="$(env "${source_env[@]}" AGENT_SESSION_CLOUD_PASSWORD='test password' \
    "$tool" cloud set "$cloud_url" --password)"
grep -Fqx 'auth=password' <<< "$password_config"
if env "${source_env[@]}" AGENT_SESSION_CLOUD_PASSWORD=$'bad\npassword' \
    "$tool" cloud set "$cloud_url" --password >/dev/null 2>&1; then
    echo 'cloud set accepted a password containing a line break' >&2
    exit 1
fi
cloud_secret="$(jq -er '.cloud.password_file' "$tmp/state/storage.json")"
[[ "$cloud_secret" == "$tmp/state/remote-passwords/neburst."*.age ]]
[[ -s "$cloud_secret" && ! -L "$cloud_secret" ]]
[[ "$(jq -r '.remotes.neburst.password_file' "$tmp/state/storage.json")" == \
   "$cloud_secret" ]]
[[ ! -e "$tmp/state/cloud-password.age" ]]
password_config_2="$(env "${source_env[@]}" \
    AGENT_SESSION_CLOUD_PASSWORD='rotated test password' \
    "$tool" cloud set "$cloud_url" --password)"
grep -Fqx 'auth=password' <<< "$password_config_2"
rotated_cloud_secret="$(jq -er '.cloud.password_file' "$tmp/state/storage.json")"
[[ "$rotated_cloud_secret" != "$cloud_secret" ]]
[[ -s "$rotated_cloud_secret" && ! -e "$cloud_secret" ]]
[[ "$(age -d -i "$tmp/key" "$rotated_cloud_secret")" == \
   'rotated test password' ]]
find "$tmp/state/trash/passwords/neburst" -maxdepth 1 -type f \
    -name "$(basename "$cloud_secret").retired.*" | grep -q .
[[ "$(find "$tmp/state/remote-passwords" -maxdepth 1 -type f \
    -name 'neburst.*.age' | wc -l)" == 1 ]]
env "${source_env[@]}" "$tool" cloud list >/dev/null
env "${source_env[@]}" "$tool" remote list | grep -Eq '^neburst +sftp +'

sync_common_env=(
    HOME="$tmp/home"
    PATH="$tmp/home/.local/bin:/usr/local/bin:/usr/bin:/bin"
    CODEX_HOME="$tmp/source/codex"
    CLAUDE_CONFIG_DIR="$tmp/source/claude"
    AGENT_SESSION_SSH_KEY="$tmp/key"
    AGENT_SESSION_DISABLE_GEO=1
)
sync_a_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$tmp/sync-a/state"
)
sync_b_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$tmp/sync-b/state"
)
sync_record_id="${queued_path##*/}"
sync_record_id="${sync_record_id%.checkpoint.tar.gz.age}"
mkdir -p "$tmp/sync-a/local/codex" "$tmp/sync-b/local" "$tmp/git"
cp -- "$queued_path" "$tmp/sync-a/local/codex/$sync_record_id.checkpoint.tar.gz.age"
git init -q --bare "$tmp/git/records.git"
git init -q -b main "$tmp/git/seed"
git -C "$tmp/git/seed" config user.name AGS-Test
git -C "$tmp/git/seed" config user.email ags-test@localhost
printf '*\n' > "$tmp/git/seed/.gitignore"
printf '* text eol=crlf\n' > "$tmp/git/seed/.gitattributes"
git -C "$tmp/git/seed" add -f -- .gitignore .gitattributes
git -C "$tmp/git/seed" commit -q -m 'seed hostile Git attributes'
git -C "$tmp/git/seed" remote add origin "$tmp/git/records.git"
git -C "$tmp/git/seed" push -q origin main
env "${sync_a_env[@]}" "$tool" set "$tmp/sync-a/local" >/dev/null

if env "${sync_a_env[@]}" "$tool" remote add insecure \
    ftp://example.test/ags >/dev/null 2>&1; then
    echo 'remote add accepted plaintext FTP' >&2
    exit 1
fi
if env "${sync_a_env[@]}" "$tool" remote add password-git git \
    https://user:secret@example.test/ags.git >/dev/null 2>&1; then
    echo 'remote add accepted an embedded Git password' >&2
    exit 1
fi
if env "${sync_a_env[@]}" "$tool" remote add token-git git \
    https://secret-token@example.test/ags.git >/dev/null 2>&1; then
    echo 'remote add accepted HTTP Git userinfo' >&2
    exit 1
fi
if env "${sync_a_env[@]}" "$tool" remote add mixed-case-token-git git \
    HTTPS://secret-token@example.test/ags.git >/dev/null 2>&1; then
    echo 'remote add accepted mixed-case HTTP Git userinfo' >&2
    exit 1
fi
if env "${sync_a_env[@]}" "$tool" remote add neburst git \
    "$tmp/git/records.git" --branch main >/dev/null 2>&1; then
    echo 'remote add accepted the SFTP-only neburst alias for Git' >&2
    exit 1
fi
if env "${sync_a_env[@]}" "$tool" remote add cloud git \
    "$tmp/git/records.git" --branch main >/dev/null 2>&1; then
    echo 'remote add accepted the SFTP-only cloud alias for Git' >&2
    exit 1
fi
git_add="$(env "${sync_a_env[@]}" "$tool" remote add backup git \
    "$tmp/git/records.git" --branch main)"
grep -Fqx 'status=configured' <<< "$git_add"
grep -Fqx 'name=backup' <<< "$git_add"
grep -Fqx 'type=git' <<< "$git_add"
git_use="$(env "${sync_a_env[@]}" "$tool" remote use backup)"
grep -Fqx 'status=selected' <<< "$git_use"
storage_modes="$(env "${sync_a_env[@]}" "$tool" storage list)"
grep -Eq '^RECENT +MODE +TYPE +LABEL$' <<< "$storage_modes"
grep -Eq '^[*] +remote:backup +git +backup Git$' <<< "$storage_modes"
storage_local="$(env "${sync_a_env[@]}" "$tool" storage use local)"
grep -Fqx 'mode=local' <<< "$storage_local"
storage_modes="$(env "${sync_a_env[@]}" "$tool" storage list)"
grep -Eq '^[*] +local +local +Local$' <<< "$storage_modes"
storage_github="$(env "${sync_a_env[@]}" "$tool" storage use github)"
grep -Fqx 'mode=remote:backup' <<< "$storage_github"
git_remotes="$(env "${sync_a_env[@]}" "$tool" remote list)"
grep -Eq '^NAME +TYPE +DEFAULT +LOCATION$' <<< "$git_remotes"
grep -Eq "^backup +git +\\* +$tmp/git/records.git$" <<< "$git_remotes"
git_show="$(env "${sync_a_env[@]}" "$tool" remote show backup)"
for expected in name=backup type=git "url=$tmp/git/records.git" branch=main default=true; do
    grep -Fqx "$expected" <<< "$git_show"
done

unsafe_sync_record_id=$'unsafe\033]52;c;AGS_SYNC\a'
unsafe_sync_path="$tmp/sync-a/local/codex/$unsafe_sync_record_id.checkpoint.tar.gz.age"
cp -- "$queued_path" "$unsafe_sync_path"
if env "${sync_a_env[@]}" "$tool" status backup \
    >"$tmp/unsafe-sync-id.out" 2>"$tmp/unsafe-sync-id.err"; then
    echo 'sync accepted a record ID containing terminal controls' >&2
    exit 1
fi
grep -Fq 'record has an unsafe ID' "$tmp/unsafe-sync-id.err"
if LC_ALL=C grep -Fq ']52;c;AGS_SYNC' "$tmp/unsafe-sync-id.err"; then
    echo 'sync error emitted an unsafe record ID' >&2
    exit 1
fi
rm -f -- "$unsafe_sync_path"

git_status="$(env "${sync_a_env[@]}" "$tool" status)"
for expected in remote=backup type=git push_records=1 pull_records=0 \
    push_tombstones=0 pull_tombstones=0 unchanged_records=0 \
    unchanged_tombstones=0; do
    grep -Fqx "$expected" <<< "$git_status"
done
git_push="$(env "${sync_a_env[@]}" "$tool" push)"
grep -Fqx 'status=synchronized' <<< "$git_push"
grep -Fqx 'pushed=1' <<< "$git_push"
grep -Fqx 'pulled=0' <<< "$git_push"
sync_record_digest="$(sha256sum "$queued_path" | cut -d' ' -f1)"
sync_v1_marker="ags-v1/records/codex/$sync_record_id.$sync_record_digest.record"
git --git-dir="$tmp/git/records.git" cat-file -e "main:$sync_v1_marker"
[[ "$(git --git-dir="$tmp/git/records.git" show main:.gitignore)" == '*' ]]
[[ "$(git --git-dir="$tmp/git/records.git" show main:.gitattributes)" == \
   '* text eol=crlf' ]]

quiet_launch_state="$tmp/quiet-launch-state"
quiet_launch_local="$tmp/quiet-launch-local"
quiet_launch_remote="$tmp/git/quiet-launch.git"
mkdir -p "$quiet_launch_state" "$quiet_launch_local/codex"
cp -- "$queued_path" \
    "$quiet_launch_local/codex/$sync_record_id.checkpoint.tar.gz.age"
git init -q --bare "$quiet_launch_remote"
git -C "$tmp/git/seed" remote add quiet-launch "$quiet_launch_remote"
git -C "$tmp/git/seed" push -q quiet-launch main
env "${source_env[@]}" \
    AGENT_SESSION_STATE_DIR="$quiet_launch_state" \
    "$tool" set "$quiet_launch_local" >/dev/null
env "${source_env[@]}" \
    AGENT_SESSION_STATE_DIR="$quiet_launch_state" \
    "$tool" remote add quiet-launch git "$quiet_launch_remote" \
        --branch main >/dev/null
quiet_remote_launch="$(
    env "${source_env[@]}" \
        AGENT_SESSION_STATE_DIR="$quiet_launch_state" \
        AGENT_SESSION_LOCAL_DIR="$quiet_launch_local" \
        AGENT_SESSION_STORAGE_MODE=remote:quiet-launch \
        "$tool" codex -- --model o3 2> "$tmp/quiet-remote-launch.err"
)"
grep -Fqx 'FAKE_CODEX <--model> <o3>' <<< "$quiet_remote_launch"
[[ ! -s "$tmp/quiet-remote-launch.err" ]]
git --git-dir="$quiet_launch_remote" cat-file -e "main:$sync_v1_marker"
git init -q --bare "$tmp/git/missing-launch.git"
env "${source_env[@]}" \
    AGENT_SESSION_STATE_DIR="$quiet_launch_state" \
    "$tool" remote add broken-launch git "$tmp/git/missing-launch.git" \
        --branch main >/dev/null
mv -- "$tmp/git/missing-launch.git" "$tmp/git/unavailable-launch.git"
broken_launches_before="$(grep -c '^LAUNCH=' "$tmp/home/.local/ags.log")"
if env "${source_env[@]}" \
    AGENT_SESSION_STATE_DIR="$quiet_launch_state" \
    AGENT_SESSION_LOCAL_DIR="$quiet_launch_local" \
    AGENT_SESSION_STORAGE_MODE=remote:broken-launch \
    "$tool" codex -- --model o3 \
        > "$tmp/broken-remote-launch.out" \
        2> "$tmp/broken-remote-launch.err"; then
    echo 'managed launch ignored an unavailable storage remote' >&2
    exit 1
fi
[[ ! -s "$tmp/broken-remote-launch.out" ]]
grep -Eq 'fatal:|\[ags\] error:' "$tmp/broken-remote-launch.err"
[[ "$broken_launches_before" == \
   "$(grep -c '^LAUNCH=' "$tmp/home/.local/ags.log")" ]]

env "${sync_b_env[@]}" "$tool" set "$tmp/sync-b/local" >/dev/null
env "${sync_b_env[@]}" "$tool" remote add backup git \
    "$tmp/git/records.git" --branch main >/dev/null
env "${sync_b_env[@]}" "$tool" remote use backup >/dev/null
git_pull_status="$(env "${sync_b_env[@]}" "$tool" status)"
grep -Fqx 'pull_records=1' <<< "$git_pull_status"
git_pull="$(env "${sync_b_env[@]}" "$tool" pull)"
grep -Fqx 'status=synchronized' <<< "$git_pull"
grep -Fqx 'pushed=0' <<< "$git_pull"
grep -Fqx 'pulled=1' <<< "$git_pull"
sync_b_record="$tmp/sync-b/local/codex/$sync_record_id.checkpoint.tar.gz.age"
cmp "$queued_path" "$sync_b_record"

auto_sync_pending="$(
    cd "$tmp/work"
    env "${sync_a_env[@]}" AGENT_SESSION_STORAGE_MODE=remote:backup \
        AGENT_SESSION_AGENT=codex AGENT_SESSION_ID="$session_id" \
        "$tool" save auto-git 'Automatic Git update'
)"
auto_sync_record_id="$(sed -n 's/^record_id=//p' <<< "$auto_sync_pending")"
auto_sync_path="$(sed -n 's/^path=//p' <<< "$auto_sync_pending")"
[[ ! -e "$auto_sync_path" ]]
printf '{"hook_event_name":"Stop","session_id":"%s"}\n' "$session_id" | \
    env "${sync_a_env[@]}" "$tool" hook
[[ -f "$auto_sync_path" ]]
auto_sync_digest="$(sha256sum "$auto_sync_path" | cut -d' ' -f1)"
auto_sync_marker="ags-v1/records/codex/$auto_sync_record_id.$auto_sync_digest.record"
git --git-dir="$tmp/git/records.git" cat-file -e "main:$auto_sync_marker"
[[ ! -d "$tmp/sync-a/state/pending" ]] ||
    ! find "$tmp/sync-a/state/pending" -type f -name '*.json' | grep -q .
auto_sync_pull="$(env "${sync_b_env[@]}" "$tool" pull backup)"
grep -Fqx 'pulled=1' <<< "$auto_sync_pull"

long_legacy_id="Legacy_$(printf '界%.0s' {1..60})_ID"
[[ "$(LC_ALL=C printf '%s' "$long_legacy_id" | wc -c)" == 190 ]]
long_legacy_payload="$tmp/long-legacy/payload"
long_legacy_archive="$tmp/long-legacy/$long_legacy_id.checkpoint.tar.gz.age"
mkdir -p "$long_legacy_payload"
cp -- "$legacy_payload/session.jsonl" "$long_legacy_payload/session.jsonl"
{
    printf 'format=2\nkind=checkpoint\ncheckpoint_id=%s\n' "$long_legacy_id"
    printf 'description=Unicode legacy sync\ncreated_utc=2026-01-03T00:00:00.000Z\ntarget=local\n'
    printf 'agent=codex\nsession_id=%s\nrelative_path=%s\n' \
        "$legacy_session_id" "$legacy_relative"
    printf 'cwd=%s\nsource_size=%s\nsource_sha256=%s\n' \
        "$tmp/work" "$(stat -c %s "$long_legacy_payload/session.jsonl")" \
        "$(sha256sum "$long_legacy_payload/session.jsonl" | cut -d' ' -f1)"
    printf 'public_ip=unknown\nip_location=unknown\ngeo_provider=unknown\n'
} > "$long_legacy_payload/manifest"
tar -C "$long_legacy_payload" -czf - manifest session.jsonl | \
    age -R "$tmp/legacy-recipient.pub" -o "$long_legacy_archive"
cp -- "$long_legacy_archive" \
    "$tmp/sync-a/local/codex/$long_legacy_id.checkpoint.tar.gz.age"
git_long_status="$(env "${sync_a_env[@]}" "$tool" status backup)"
grep -Fqx 'push_records=1' <<< "$git_long_status"
git_long_push="$(env "${sync_a_env[@]}" "$tool" push backup)"
grep -Fqx 'pushed=1' <<< "$git_long_push"
long_legacy_id_digest="$(printf '%s' "$long_legacy_id" | sha256sum | cut -d' ' -f1)"
long_legacy_record_digest="$(sha256sum "$long_legacy_archive" | cut -d' ' -f1)"
long_legacy_marker="ags-v1/records/codex/id-$long_legacy_id_digest.$long_legacy_record_digest.record"
git --git-dir="$tmp/git/records.git" cat-file -e "main:$long_legacy_marker"
git --git-dir="$tmp/git/records.git" show "main:$long_legacy_marker" \
    > "$tmp/long-legacy/marker"
[[ "$(cut -f2 "$tmp/long-legacy/marker")" == "$long_legacy_id" ]]
git_long_pull="$(env "${sync_b_env[@]}" "$tool" pull backup)"
grep -Fqx 'pulled=1' <<< "$git_long_pull"
sync_b_long_record="$tmp/sync-b/local/codex/$long_legacy_id.checkpoint.tar.gz.age"
cmp "$long_legacy_archive" "$sync_b_long_record"
git_long_show="$(env "${sync_b_env[@]}" "$tool" show "codex/$long_legacy_id")"
grep -Eq "^Selector +codex/$long_legacy_id$" <<< "$git_long_show"
grep -Eq '^Format +2$' <<< "$git_long_show"

printf 'different bytes\n' > "$sync_b_record"
if env "${sync_b_env[@]}" "$tool" status backup \
    >"$tmp/git-conflict.out" 2>"$tmp/git-conflict.err"; then
    echo 'Git status accepted different bytes for the same record ID' >&2
    exit 1
fi
grep -Fq 'E_SYNC_CONFLICT' "$tmp/git-conflict.err"
grep -Fq "record_id=$sync_record_id" "$tmp/git-conflict.err"
cp -- "$queued_path" "$sync_b_record"

sync_delete="$(env "${sync_a_env[@]}" "$tool" delete active-window)"
grep -Fqx 'status=deleted' <<< "$sync_delete"
grep -Fqx 'sync_status=synchronized' <<< "$sync_delete"
grep -Fqx 'storage_mode=remote:backup' <<< "$sync_delete"
sync_tombstone="$(sed -n 's/^tombstone=//p' <<< "$sync_delete")"
[[ -f "$sync_tombstone" ]]
sync_tombstone_digest="$(sha256sum "$sync_tombstone" | cut -d' ' -f1)"
sync_tombstone_marker="ags-v1/tombstones/codex/$sync_record_id.$sync_tombstone_digest.tombstone"
git --git-dir="$tmp/git/records.git" cat-file -e "main:$sync_tombstone_marker"
tombstone_push="$(env "${sync_a_env[@]}" "$tool" push backup)"
grep -Fqx 'push_tombstones=0' <<< "$tombstone_push"
grep -Fqx 'status=synchronized' <<< "$tombstone_push"
tombstone_pull="$(env "${sync_b_env[@]}" "$tool" pull backup)"
grep -Fqx 'pull_tombstones=1' <<< "$tombstone_pull"
grep -Fqx 'status=synchronized' <<< "$tombstone_pull"
[[ ! -e "$sync_b_record" ]]
[[ -f "$tmp/sync-b/local/tombstones/codex/$sync_record_id.tombstone" ]]
find "$tmp/sync-b/state/trash/sync/codex" -type f \
    -name "$sync_record_id.checkpoint.tar.gz.age.deleted.*" | grep -q .
! env "${sync_b_env[@]}" "$tool" list | grep -Fq active-window

mv -- "$tmp/git/records.git" "$tmp/git/records.offline.git"
if pending_delete="$(env "${sync_a_env[@]}" "$tool" delete auto-git \
    2>"$tmp/pending-delete.err")"; then
    pending_delete_status=0
else
    pending_delete_status=$?
fi
mv -- "$tmp/git/records.offline.git" "$tmp/git/records.git"
(( pending_delete_status != 0 ))
grep -Fqx 'status=deleted' <<< "$pending_delete"
grep -Fqx 'sync_status=pending' <<< "$pending_delete"
grep -Fqx 'storage_mode=remote:backup' <<< "$pending_delete"
pending_sync_path="$(sed -n 's/^pending_sync=//p' <<< "$pending_delete")"
[[ -f "$pending_sync_path" ]]
env "${sync_a_env[@]}" "$tool" flush
[[ ! -e "$pending_sync_path" ]]
auto_sync_tombstone="$tmp/sync-a/local/tombstones/codex/$auto_sync_record_id.tombstone"
auto_sync_tombstone_digest="$(sha256sum "$auto_sync_tombstone" | cut -d' ' -f1)"
auto_sync_tombstone_marker="ags-v1/tombstones/codex/$auto_sync_record_id.$auto_sync_tombstone_digest.tombstone"
git --git-dir="$tmp/git/records.git" cat-file -e \
    "main:$auto_sync_tombstone_marker"
pending_delete_pull="$(env "${sync_b_env[@]}" "$tool" pull backup)"
grep -Fqx 'pull_tombstones=1' <<< "$pending_delete_pull"
[[ ! -e "$tmp/sync-b/local/codex/$auto_sync_record_id.checkpoint.tar.gz.age" ]]

git_sync="$(env "${sync_b_env[@]}" "$tool" sync backup)"
grep -Fqx 'status=synchronized' <<< "$git_sync"
grep -Fqx 'pushed=0' <<< "$git_sync"
grep -Fqx 'pulled=0' <<< "$git_sync"
git_long_delete="$(env "${sync_a_env[@]}" "$tool" delete "codex/$long_legacy_id")"
grep -Fqx 'sync_status=synchronized' <<< "$git_long_delete"
grep -Fqx 'storage_mode=remote:backup' <<< "$git_long_delete"
long_legacy_tombstone="$(sed -n 's/^tombstone=//p' <<< "$git_long_delete")"
long_legacy_tombstone_digest="$(sha256sum "$long_legacy_tombstone" | cut -d' ' -f1)"
long_legacy_tombstone_marker="ags-v1/tombstones/codex/id-$long_legacy_id_digest.$long_legacy_tombstone_digest.tombstone"
git_long_tombstone_push="$(env "${sync_a_env[@]}" "$tool" push backup)"
grep -Fqx 'push_tombstones=0' <<< "$git_long_tombstone_push"
git --git-dir="$tmp/git/records.git" cat-file -e "main:$long_legacy_tombstone_marker"
git_long_tombstone_pull="$(env "${sync_b_env[@]}" "$tool" pull backup)"
grep -Fqx 'pull_tombstones=1' <<< "$git_long_tombstone_pull"
[[ ! -e "$sync_b_long_record" ]]

sftp_state="$tmp/sftp-sync/state"
sftp_local="$tmp/sftp-sync/local"
sftp_remote="$tmp/sftp-sync/remote"
sftp_log="$tmp/sftp-sync/rclone.log"
sftp_ssh_log="$tmp/sftp-sync/ssh.log"
mkdir -p "$sftp_local/codex" "$sftp_remote" "$(dirname "$sftp_log")"
sftp_record_id="${zst_path##*/}"
sftp_record_id="${sftp_record_id%.checkpoint.tar.gz.age}"
cp -- "$zst_path" "$sftp_local/codex/$sftp_record_id.checkpoint.tar.gz.age"
sftp_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$sftp_state"
    FAKE_RCLONE_ROOT="$sftp_remote"
    FAKE_RCLONE_LOG="$sftp_log"
    FAKE_SSH_LOG="$sftp_ssh_log"
)
env "${sftp_env[@]}" "$tool" set "$sftp_local" >/dev/null
: > "$tmp/sftp-sync/untrusted-known-hosts"
sftp_url='sftp://tester@127.0.0.1:2222/sync-records'
if env "${sftp_env[@]}" "$tool" remote add rejected "$sftp_url" \
    --known-hosts "$tmp/sftp-sync/untrusted-known-hosts" --key "$tmp/key" \
    >"$tmp/sftp-host.out" 2>"$tmp/sftp-host.err"; then
    echo 'SFTP remote accepted a host missing from known_hosts' >&2
    exit 1
fi
grep -Fq 'no verified host key for [127.0.0.1]:2222' "$tmp/sftp-host.err"
read -r host_key_type host_key_data _ < "$tmp/key.pub"
printf '[127.0.0.1]:2222 %s %s\n' "$host_key_type" "$host_key_data" \
    > "$tmp/sftp-sync/known_hosts"
if env "${sftp_env[@]}" "$tool" remote add github "$sftp_url" \
    --known-hosts "$tmp/sftp-sync/known_hosts" --key "$tmp/key" \
    >/dev/null 2>&1; then
    echo 'remote add accepted the Git-only github alias for SFTP' >&2
    exit 1
fi
mkdir -p "$tmp/sftp-sync/outside-passwords"
ln -s "$tmp/sftp-sync/outside-passwords" "$sftp_state/remote-passwords"
if env "${sftp_env[@]}" AGENT_SESSION_REMOTE_PASSWORD='must stay inside' \
    "$tool" remote add symlink-secret "$sftp_url" \
        --known-hosts "$tmp/sftp-sync/known_hosts" --password \
        >/dev/null 2>&1; then
    echo 'SFTP password publication followed a state symlink' >&2
    exit 1
fi
! find "$tmp/sftp-sync/outside-passwords" -type f | grep -q .
rm -- "$sftp_state/remote-passwords"
named_password='synthetic Neburst password'
: > "$sftp_log"
: > "$sftp_ssh_log"
neburst_add="$(env "${sftp_env[@]}" \
    AGENT_SESSION_REMOTE_PASSWORD="$named_password" \
    "$tool" remote add neburst "$sftp_url" \
        --known-hosts "$tmp/sftp-sync/known_hosts" --password)"
grep -Fqx 'status=configured' <<< "$neburst_add"
grep -Fqx 'auth=password' <<< "$neburst_add"
neburst_secret="$sftp_state/remote-passwords/neburst.age"
[[ -s "$neburst_secret" ]]
[[ "$(age -d -i "$tmp/key" "$neburst_secret")" == "$named_password" ]]
! grep -Fq "$named_password" "$sftp_state/storage.json"
! grep -Fq "$named_password" "$sftp_log"
! grep -Fq "$named_password" "$sftp_ssh_log"
! grep -Fq 'pass=' "$sftp_log"
! grep -Fq 'PLAINTEXT_ENV_LEAK=1' "$sftp_ssh_log"
! grep -Fq 'CLOUD_PASSWORD_ENV_LEAK=1' "$sftp_ssh_log"
! grep -Fq 'RCLONE_PASSWORD_ENV_LEAK=1' "$sftp_ssh_log"
grep -Fq $'AUTH_ENV=1' "$sftp_log"
grep -Fq $'sshpass\tPASSWORD_FD=1\tPASSWORD_SHA256=' "$sftp_ssh_log"
neburst_remove="$(env "${sftp_env[@]}" "$tool" remote remove neburst)"
neburst_recovery="$(sed -n 's/^recoverable_config=//p' <<< "$neburst_remove")"
[[ -s "$neburst_recovery/password.age" && ! -e "$neburst_secret" ]]
[[ "$(jq -r '.password_file' "$neburst_recovery/config.json")" == \
   "$neburst_recovery/password.age" ]]

sftp_add="$(env "${sftp_env[@]}" "$tool" remote add sftp-backup "$sftp_url" \
    --known-hosts "$tmp/sftp-sync/known_hosts" --key "$tmp/key")"
grep -Fqx 'status=configured' <<< "$sftp_add"
grep -Fqx 'type=sftp' <<< "$sftp_add"
env "${sftp_env[@]}" "$tool" remote use sftp-backup >/dev/null
: > "$sftp_log"
: > "$sftp_ssh_log"
sftp_push="$(env "${sftp_env[@]}" "$tool" push)"
grep -Fqx 'type=sftp' <<< "$sftp_push"
grep -Fqx 'push_records=1' <<< "$sftp_push"
grep -Fqx 'status=synchronized' <<< "$sftp_push"
object_publish_line="$(awk \
    '/STDIN=ags-v1\/objects\/.*\.checkpoint\.tar\.gz\.age/ {print NR; exit}' \
    "$sftp_ssh_log")"
marker_publish_line="$(awk \
    '/STDIN=ags-v1\/records\/codex\/.*\.record/ {print NR; exit}' \
    "$sftp_ssh_log")"
[[ "$object_publish_line" =~ ^[0-9]+$ && "$marker_publish_line" =~ ^[0-9]+$ ]]
(( object_publish_line < marker_publish_line ))
find "$sftp_remote/sync-records/ags-v1/objects" -type f \
    -name '*.checkpoint.tar.gz.age' | grep -q .
find "$sftp_remote/sync-records/ags-v1/records/codex" -type f \
    -name '*.record' | grep -q .
sftp_record_digest="$(sha256sum "$zst_path" | cut -d' ' -f1)"
sftp_v1_marker="ags-v1/records/codex/$sftp_record_id.$sftp_record_digest.record"
[[ -f "$sftp_remote/sync-records/$sftp_v1_marker" ]]
cp -- "$long_legacy_archive" \
    "$sftp_local/codex/$long_legacy_id.checkpoint.tar.gz.age"
sftp_long_push="$(env "${sftp_env[@]}" "$tool" push)"
grep -Fqx 'push_records=1' <<< "$sftp_long_push"
[[ -f "$sftp_remote/sync-records/$long_legacy_marker" ]]
[[ "$(cut -f2 "$sftp_remote/sync-records/$long_legacy_marker")" == "$long_legacy_id" ]]
sftp_long_status="$(env "${sftp_env[@]}" "$tool" status)"
grep -Fqx 'unchanged_records=2' <<< "$sftp_long_status"
sftp_show="$(env "${sftp_env[@]}" "$tool" remote show sftp-backup)"
grep -Fqx 'default=true' <<< "$sftp_show"
grep -Fqx "known_hosts=$tmp/sftp-sync/known_hosts" <<< "$sftp_show"

mkdir -p "$tmp/sftp-sync/operation-hold"
env "${sftp_env[@]}" \
    FAKE_RCLONE_LSF_HOLD_DIR="$tmp/sftp-sync/operation-hold" \
    "$tool" status sftp-backup \
    >"$tmp/sftp-sync/held-status.out" \
    2>"$tmp/sftp-sync/held-status.err" &
held_status_pid=$!
hold_ready=0
for _ in {1..500}; do
    if [[ -e "$tmp/sftp-sync/operation-hold/ready" ]]; then
        hold_ready=1
        break
    fi
    sleep 0.01
done
(( hold_ready == 1 ))
env "${sftp_env[@]}" "$tool" remote remove sftp-backup \
    >"$tmp/sftp-sync/concurrent-remove.out" \
    2>"$tmp/sftp-sync/concurrent-remove.err" &
concurrent_remove_pid=$!
remove_waiting=0
for _ in {1..100}; do
    if kill -0 "$concurrent_remove_pid" 2>/dev/null; then
        remove_waiting=1
        break
    fi
    sleep 0.01
done
(( remove_waiting == 1 ))
[[ -s "$sftp_state/storage.json" ]]
jq -e '.remotes["sftp-backup"] != null' \
    "$sftp_state/storage.json" >/dev/null
: > "$tmp/sftp-sync/operation-hold/release"
wait "$held_status_pid"
wait "$concurrent_remove_pid"
grep -Fqx 'status=removed' "$tmp/sftp-sync/concurrent-remove.out"
jq -e '.remotes["sftp-backup"] == null' \
    "$sftp_state/storage.json" >/dev/null
[[ ! -d "$sftp_state/pending-sync" ]] ||
    ! find "$sftp_state/pending-sync" -type f -name '*.json' | grep -q .
env "${sftp_env[@]}" "$tool" remote add sftp-backup "$sftp_url" \
    --known-hosts "$tmp/sftp-sync/known_hosts" --key "$tmp/key" >/dev/null

merge_conflict_state="$tmp/merge-conflict/state"
merge_conflict_local="$tmp/merge-conflict/local"
merge_conflict_seed_state="$tmp/merge-conflict/seed-state"
merge_conflict_seed_local="$tmp/merge-conflict/seed-local"
merge_conflict_git="$tmp/merge-conflict/source.git"
merge_conflict_destination_git="$tmp/merge-conflict/destination.git"
merge_conflict_destination_work="$tmp/merge-conflict/destination-work"
mkdir -p "$merge_conflict_local/codex" "$merge_conflict_seed_local/codex"
git init -q --bare "$merge_conflict_git"
git init -q --bare "$merge_conflict_destination_git"
git init -q "$merge_conflict_destination_work"
git -C "$merge_conflict_destination_work" config user.name AGS-Test
git -C "$merge_conflict_destination_work" config user.email ags-test@localhost
printf 'pre-existing destination history\n' \
    > "$merge_conflict_destination_work/README"
git -C "$merge_conflict_destination_work" add README
git -C "$merge_conflict_destination_work" commit -qm 'seed destination'
git -C "$merge_conflict_destination_work" remote add origin \
    "$merge_conflict_destination_git"
git -C "$merge_conflict_destination_work" push -q origin HEAD:main
cp -- "$zst_path" \
    "$merge_conflict_local/codex/$sftp_record_id.checkpoint.tar.gz.age"
printf 'different encrypted checkpoint bytes\n' \
    > "$merge_conflict_seed_local/codex/$sftp_record_id.checkpoint.tar.gz.age"
merge_conflict_seed_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$merge_conflict_seed_state"
)
env "${merge_conflict_seed_env[@]}" "$tool" set \
    "$merge_conflict_seed_local" >/dev/null
env "${merge_conflict_seed_env[@]}" "$tool" remote add conflict-source git \
    "$merge_conflict_git" --branch main >/dev/null
env "${merge_conflict_seed_env[@]}" "$tool" push conflict-source >/dev/null
merge_conflict_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$merge_conflict_state"
)
env "${merge_conflict_env[@]}" "$tool" set "$merge_conflict_local" >/dev/null
env "${merge_conflict_env[@]}" "$tool" remote add conflict-source git \
    "$merge_conflict_git" --branch main >/dev/null
env "${merge_conflict_env[@]}" "$tool" remote add conflict-destination git \
    "$merge_conflict_destination_git" --branch main >/dev/null
cp -- "$merge_conflict_state/storage.json" \
    "$tmp/merge-conflict/storage.before.json"
merge_conflict_local_before="$(
    {
        find "$merge_conflict_local" -mindepth 1 ! -name .ags.lock \
            -printf '%y\t%P\n' | sort
        find "$merge_conflict_local" -type f ! -name .ags.lock -print0 |
            sort -z | xargs -0 sha256sum
    }
)"
merge_conflict_remote_before="$(
    git --git-dir="$merge_conflict_git" rev-parse main
)"
merge_conflict_destination_before="$(
    git --git-dir="$merge_conflict_destination_git" rev-parse main
)"
if env "${merge_conflict_env[@]}" "$tool" storage merge \
    --into conflict-destination conflict-source \
    >"$tmp/merge-conflict/merge.out" \
    2>"$tmp/merge-conflict/merge.err"; then
    echo 'storage merge accepted different bytes for the same record ID' >&2
    exit 1
fi
grep -Fq 'E_SYNC_CONFLICT' "$tmp/merge-conflict/merge.err"
grep -Fq "record_id=$sftp_record_id" "$tmp/merge-conflict/merge.err"
[[ "$merge_conflict_local_before" == "$(
    {
        find "$merge_conflict_local" -mindepth 1 ! -name .ags.lock \
            -printf '%y\t%P\n' | sort
        find "$merge_conflict_local" -type f ! -name .ags.lock -print0 |
            sort -z | xargs -0 sha256sum
    }
)" ]]
[[ "$merge_conflict_remote_before" == \
   "$(git --git-dir="$merge_conflict_git" rev-parse main)" ]]
[[ "$merge_conflict_destination_before" == \
   "$(git --git-dir="$merge_conflict_destination_git" rev-parse main)" ]]
cmp "$tmp/merge-conflict/storage.before.json" \
    "$merge_conflict_state/storage.json"
jq -e '((.merged_modes // {}) | length) == 0' \
    "$merge_conflict_state/storage.json" >/dev/null

legacy_retire_state="$tmp/legacy-retire/state"
legacy_retire_local="$tmp/legacy-retire/local"
legacy_retire_remote="$tmp/legacy-retire/remote"
legacy_retire_final="$tmp/legacy-retire/final.git"
mkdir -p "$legacy_retire_local" "$legacy_retire_remote"
git init -q --bare "$legacy_retire_final"
legacy_retire_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$legacy_retire_state"
    FAKE_RCLONE_ROOT="$legacy_retire_remote"
    FAKE_RCLONE_LOG="$tmp/legacy-retire/rclone.log"
    FAKE_SSH_LOG="$tmp/legacy-retire/ssh.log"
)
legacy_retire_url='sftp://tester@127.0.0.1:2222/legacy-only'
env "${legacy_retire_env[@]}" "$tool" set "$legacy_retire_local" >/dev/null
env "${legacy_retire_env[@]}" "$tool" cloud set \
    "$legacy_retire_url" --key "$tmp/key" >/dev/null
mkdir -p "$legacy_retire_remote/legacy-only/codex"
cp -- "$long_legacy_archive" \
    "$legacy_retire_remote/legacy-only/codex/$long_legacy_id.checkpoint.tar.gz.age"
env "${legacy_retire_env[@]}" "$tool" remote add final git \
    "$legacy_retire_final" --branch main >/dev/null
legacy_only_merge="$(env "${legacy_retire_env[@]}" "$tool" storage merge \
    --into final neburst)"
grep -Fqx 'source=remote:neburst' <<< "$legacy_only_merge"
if env "${legacy_retire_env[@]}" \
    FAKE_SSH_FAIL_RETIRE_ONCE_MARKER="$tmp/legacy-retire/server.failed" \
    "$tool" storage retire neburst --into final \
    >"$tmp/legacy-retire/first.out" 2>"$tmp/legacy-retire/first.err"; then
    echo 'legacy-only retirement ignored a failed server transaction' >&2
    exit 1
fi
legacy_retire_timestamp="$(
    jq -er '.remotes.neburst.retiring.timestamp' \
        "$legacy_retire_state/storage.json"
)"
legacy_partial_dir="$legacy_retire_remote/legacy-only/.ags-retired/$legacy_retire_timestamp/legacy-cloud"
mkdir -p "$legacy_partial_dir"
mv -- "$legacy_retire_remote/legacy-only/codex" \
    "$legacy_partial_dir/codex"
legacy_partial_archive="$legacy_partial_dir/codex/$long_legacy_id.checkpoint.tar.gz.age"
cp -- "$legacy_partial_archive" "$tmp/legacy-retire/legacy.original"
printf 'partial-retirement-tamper\n' >> "$legacy_partial_archive"
if env "${legacy_retire_env[@]}" "$tool" storage retire \
    neburst --into final \
    >"$tmp/legacy-retire/partial.out" \
    2>"$tmp/legacy-retire/partial.err"; then
    echo 'legacy-only partial retirement was incorrectly cancelled as untouched' >&2
    exit 1
fi
grep -Fq 'fail-closed transaction was retained' \
    "$tmp/legacy-retire/partial.err"
jq -e '.remotes.neburst.retiring != null' \
    "$legacy_retire_state/storage.json" >/dev/null
cp -- "$tmp/legacy-retire/legacy.original" "$legacy_partial_archive"
legacy_retire_done="$(env "${legacy_retire_env[@]}" "$tool" storage retire \
    neburst --into final)"
grep -Fqx 'status=retired' <<< "$legacy_retire_done"
grep -Fqx 'source=remote:neburst' <<< "$legacy_retire_done"
[[ ! -e "$legacy_retire_remote/legacy-only/codex" ]]
[[ -f "$legacy_retire_remote/legacy-only/.ags-retired/ags-v1.retired" ]]
jq -e '.cloud == null and .remotes.neburst == null' \
    "$legacy_retire_state/storage.json" >/dev/null

merge_state="$tmp/storage-merge/state"
merge_local="$tmp/storage-merge/local"
merge_final_git="$tmp/storage-merge/final.git"
mkdir -p "$merge_local"
git init -q --bare "$merge_final_git"
merge_env=(
    "${sync_common_env[@]}"
    AGENT_SESSION_STATE_DIR="$merge_state"
    FAKE_RCLONE_ROOT="$sftp_remote"
    FAKE_RCLONE_LOG="$tmp/storage-merge/rclone.log"
)
env "${merge_env[@]}" "$tool" set "$merge_local" >/dev/null
env "${merge_env[@]}" "$tool" remote add backup git \
    "$tmp/git/records.git" --branch main >/dev/null
env "${merge_env[@]}" "$tool" remote add sftp-backup "$sftp_url" \
    --known-hosts "$tmp/sftp-sync/known_hosts" --key "$tmp/key" >/dev/null
env "${merge_env[@]}" "$tool" remote add final git \
    "$merge_final_git" --branch main >/dev/null
env "${merge_env[@]}" "$tool" storage use backup >/dev/null
env "${merge_env[@]}" "$tool" storage use sftp-backup >/dev/null
env "${merge_env[@]}" "$tool" storage use local >/dev/null
env "${merge_env[@]}" "$tool" storage use final >/dev/null
merge_modes_before="$(env "${merge_env[@]}" "$tool" storage list)"
mapfile -t merge_mode_order < <(
    awk 'NR > 1 {if ($1 == "*") print $2; else print $1}' \
        <<< "$merge_modes_before"
)
[[ "${merge_mode_order[*]}" == \
   'remote:final local remote:sftp-backup remote:backup' ]]
mkdir -p "$tmp/storage-merge/operation-hold"
env "${merge_env[@]}" \
    FAKE_RCLONE_LSF_HOLD_DIR="$tmp/storage-merge/operation-hold" \
    "$tool" storage merge --into final backup sftp-backup \
    >"$tmp/storage-merge/merge.out" \
    2>"$tmp/storage-merge/merge.err" &
storage_merge_pid=$!
test_child_pids["$storage_merge_pid"]=1
merge_hold_ready=0
for _ in {1..500}; do
    if [[ -e "$tmp/storage-merge/operation-hold/ready" ]]; then
        merge_hold_ready=1
        break
    fi
    sleep 0.01
done
(( merge_hold_ready == 1 ))
(
    cd "$tmp/work"
    env "${merge_env[@]}" AGENT_SESSION_STORAGE_MODE=remote:backup \
        "$tool" save-now local codex "$session_id" \
            started-before-merge 'Started before storage redirect'
) >"$tmp/storage-merge/concurrent-save.out" \
    2>"$tmp/storage-merge/concurrent-save.err" &
merge_save_pid=$!
test_child_pids["$merge_save_pid"]=1
merge_save_waiting=0
for _ in {1..500}; do
    if test_process_has_fd_target "$merge_save_pid" \
        "$merge_state/storage-consolidation.lock"; then
        merge_save_waiting=1
        break
    fi
    test_process_running "$merge_save_pid" || break
    sleep 0.01
done
(( merge_save_waiting == 1 ))
test_process_running "$storage_merge_pid"
test_process_running "$merge_save_pid"
: > "$tmp/storage-merge/operation-hold/release"
merge_finished=0
for _ in {1..3000}; do
    if ! test_process_running "$storage_merge_pid" &&
       ! test_process_running "$merge_save_pid"; then
        merge_finished=1
        break
    fi
    sleep 0.01
done
(( merge_finished == 1 ))
wait "$storage_merge_pid"
unset "test_child_pids[$storage_merge_pid]"
wait "$merge_save_pid"
unset "test_child_pids[$merge_save_pid]"
storage_merge="$(<"$tmp/storage-merge/merge.out")"
grep -Fqx 'status=merged' <<< "$storage_merge"
grep -Fqx 'destination=remote:final' <<< "$storage_merge"
grep -Fqx 'source=remote:backup' <<< "$storage_merge"
grep -Fqx 'source=remote:sftp-backup' <<< "$storage_merge"
grep -Fq 'retire_command=ags storage retire remote:sftp-backup --into remote:final' \
    <<< "$storage_merge"
concurrent_save="$(<"$tmp/storage-merge/concurrent-save.out")"
grep -Fqx 'storage_mode=remote:final' <<< "$concurrent_save"
concurrent_record_id="$(sed -n 's/^record_id=//p' <<< "$concurrent_save")"
concurrent_path="$(sed -n 's/^path=//p' <<< "$concurrent_save")"
concurrent_digest="$(sha256sum "$concurrent_path" | cut -d' ' -f1)"
concurrent_marker="ags-v1/records/codex/$concurrent_record_id.$concurrent_digest.record"
git --git-dir="$merge_final_git" cat-file -e "main:$concurrent_marker"
if git --git-dir="$tmp/git/records.git" cat-file -e \
    "main:$concurrent_marker" 2>/dev/null; then
    echo 'save started before merge repopulated its stale source replica' >&2
    exit 1
fi
merge_final_status="$(env "${merge_env[@]}" "$tool" status final)"
for expected in push_records=0 pull_records=0 \
    push_tombstones=0 pull_tombstones=0; do
    grep -Fqx "$expected" <<< "$merge_final_status"
done
find "$merge_local" -type f -name '*.checkpoint.tar.gz.age' | grep -q .
merge_modes_after="$(env "${merge_env[@]}" "$tool" storage list)"
grep -Eq '^[*] +remote:final +git +final Git$' <<< "$merge_modes_after"
! grep -Fq 'remote:backup' <<< "$merge_modes_after"
! grep -Fq 'remote:sftp-backup' <<< "$merge_modes_after"
if env "${merge_env[@]}" "$tool" remote remove backup \
    >"$tmp/storage-merge/remove-merged.out" \
    2>"$tmp/storage-merge/remove-merged.err"; then
    echo 'remote remove discarded an active merge redirect' >&2
    exit 1
fi
grep -Fq 'participates in storage consolidation' \
    "$tmp/storage-merge/remove-merged.err"
redirected_save="$(
    cd "$tmp/work"
    env "${merge_env[@]}" AGENT_SESSION_STORAGE_MODE=remote:backup \
        "$tool" save-now local codex "$session_id" \
            redirected-after-merge 'Redirected merged storage'
)"
grep -Fqx 'storage_mode=remote:final' <<< "$redirected_save"
redirected_record_id="$(sed -n 's/^record_id=//p' <<< "$redirected_save")"
redirected_path="$(sed -n 's/^path=//p' <<< "$redirected_save")"
redirected_digest="$(sha256sum "$redirected_path" | cut -d' ' -f1)"
redirected_marker="ags-v1/records/codex/$redirected_record_id.$redirected_digest.record"
git --git-dir="$merge_final_git" cat-file -e "main:$redirected_marker"
if git --git-dir="$tmp/git/records.git" cat-file -e \
    "main:$redirected_marker" 2>/dev/null; then
    echo 'old merged mode wrote to its stale source replica' >&2
    exit 1
fi
if env "${merge_env[@]}" \
    FAKE_SSH_FAIL_RETIRE_ONCE_MARKER="$tmp/storage-merge/retire-server.failed" \
    "$tool" storage retire sftp-backup --into final \
    >"$tmp/storage-merge/retire-server.out" \
    2>"$tmp/storage-merge/retire-server.err"; then
    echo 'SFTP retirement ignored a failed server transaction' >&2
    exit 1
fi
jq -e '.remotes["sftp-backup"].retiring != null' \
    "$merge_state/storage.json" >/dev/null
cp -- "$merge_state/storage.json" \
    "$tmp/storage-merge/storage.before-recovery-path.json"
jq '.remotes["sftp-backup"].retiring.recoverable_config =
    (.remotes["sftp-backup"].retiring.recoverable_config + "/../escape")' \
    "$merge_state/storage.json" > "$tmp/storage-merge/storage.invalid.json"
mv -- "$tmp/storage-merge/storage.invalid.json" "$merge_state/storage.json"
if env "${merge_env[@]}" "$tool" storage retire sftp-backup --into final \
    >/dev/null 2>"$tmp/storage-merge/recovery-traversal.err"; then
    echo 'retirement accepted a recovery path containing traversal' >&2
    exit 1
fi
grep -Fq 'retirement recovery data is unavailable' \
    "$tmp/storage-merge/recovery-traversal.err"
cp -- "$tmp/storage-merge/storage.before-recovery-path.json" \
    "$merge_state/storage.json"
recovery_path="$(jq -er \
    '.remotes["sftp-backup"].retiring.recoverable_config' \
    "$merge_state/storage.json")"
recovery_parent="$(dirname "$recovery_path")"
mv -- "$recovery_parent" "$recovery_parent.safe"
mkdir -p "$tmp/storage-merge/recovery-outside"
ln -s "$tmp/storage-merge/recovery-outside" "$recovery_parent"
if env "${merge_env[@]}" "$tool" storage retire sftp-backup --into final \
    >/dev/null 2>"$tmp/storage-merge/recovery-symlink.err"; then
    echo 'retirement followed a symbolic-link recovery ancestor' >&2
    exit 1
fi
grep -Fq 'retirement recovery data is unavailable' \
    "$tmp/storage-merge/recovery-symlink.err"
rm -- "$recovery_parent"
mv -- "$recovery_parent.safe" "$recovery_parent"
if env "${merge_env[@]}" "$tool" storage merge --into backup final \
    >"$tmp/storage-merge/merge-during-retire.out" \
    2>"$tmp/storage-merge/merge-during-retire.err"; then
    echo 'storage merge ran while another retirement was interrupted' >&2
    exit 1
fi
grep -Fq 'interrupted storage retirement' \
    "$tmp/storage-merge/merge-during-retire.err"
tampered_sftp_file="$(
    find "$sftp_remote/sync-records/ags-v1/objects" -type f | head -n 1
)"
[[ -n "$tampered_sftp_file" ]]
cp -- "$tampered_sftp_file" "$tmp/storage-merge/tampered-object.original"
printf 'tamper\n' >> "$tampered_sftp_file"
if env "${merge_env[@]}" "$tool" storage retire \
    sftp-backup --into final \
    >"$tmp/storage-merge/retire-tamper.out" \
    2>"$tmp/storage-merge/retire-tamper.err"; then
    echo 'SFTP retirement accepted content outside its verified revision' >&2
    exit 1
fi
grep -Fq 'remote advanced before retirement' \
    "$tmp/storage-merge/retire-tamper.err"
jq -e '.remotes["sftp-backup"].retiring == null' \
    "$merge_state/storage.json" >/dev/null
cmp -s "$tampered_sftp_file" "$tmp/storage-merge/tampered-object.original" && {
    echo 'SFTP tamper fixture did not change the remote object' >&2
    exit 1
}
cp -- "$tmp/storage-merge/tampered-object.original" "$tampered_sftp_file"

if env "${merge_env[@]}" \
    FAKE_RCLONE_FAIL_RETIRE_READBACK_ONCE="$tmp/storage-merge/retire-readback.failed" \
    "$tool" storage retire sftp-backup --into final \
    >"$tmp/storage-merge/retire-readback.out" \
    2>"$tmp/storage-merge/retire-readback.err"; then
    echo 'SFTP retirement ignored a failed marker read-back' >&2
    exit 1
fi
[[ -f "$sftp_remote/sync-records/.ags-retired/ags-v1.retired" ]]
jq -e '.remotes["sftp-backup"].retiring != null' \
    "$merge_state/storage.json" >/dev/null
storage_retire="$(env "${merge_env[@]}" "$tool" storage retire \
    sftp-backup --into final)"
grep -Fqx 'status=retired' <<< "$storage_retire"
grep -Fqx 'source=remote:sftp-backup' <<< "$storage_retire"
retired_sftp_path="$(sed -n 's/^recoverable_replica=//p' <<< "$storage_retire")"
[[ "$retired_sftp_path" == "$sftp_url/"* ]]
[[ ! -d "$sftp_remote/sync-records/ags-v1" ]]
find "$sftp_remote/sync-records/.ags-retired" -type f | grep -q .
! env "${merge_env[@]}" "$tool" remote list | grep -Fq sftp-backup
if env "${sftp_env[@]}" "$tool" push sftp-backup \
    >"$tmp/storage-merge/retired-push.out" \
    2>"$tmp/storage-merge/retired-push.err"; then
    echo 'SFTP sync resurrected a globally retired replica' >&2
    exit 1
fi
grep -Fq 'retired' "$tmp/storage-merge/retired-push.err"
if env "${merge_env[@]}" "$tool" remote add sftp-reborn "$sftp_url" \
    --known-hosts "$tmp/sftp-sync/known_hosts" --key "$tmp/key" \
    >/dev/null 2>&1; then
    echo 'remote add resurrected a retired SFTP replica' >&2
    exit 1
fi

final_before_retire="$(git --git-dir="$merge_final_git" rev-parse main)"
final_tree_before_retire="$(
    git --git-dir="$merge_final_git" rev-parse main:ags-v1
)"
reverse_merge="$(env "${merge_env[@]}" "$tool" storage merge \
    --into backup final)"
grep -Fqx 'status=merged' <<< "$reverse_merge"
grep -Fqx 'destination=remote:backup' <<< "$reverse_merge"
grep -Fqx 'source=remote:final' <<< "$reverse_merge"
jq -e '
    .merged_modes["remote:final"] == "remote:backup" and
    .merged_modes["remote:backup"] == null and
    .retired_modes["remote:sftp-backup"] == "remote:backup"
' "$merge_state/storage.json" >/dev/null
reverse_modes="$(env "${merge_env[@]}" "$tool" storage list)"
grep -Eq '^[*] +remote:backup +git +backup Git$' <<< "$reverse_modes"
! grep -Fq 'remote:final' <<< "$reverse_modes"
reverse_redirect_save="$(
    cd "$tmp/work"
    env "${merge_env[@]}" AGENT_SESSION_STORAGE_MODE=remote:final \
        "$tool" save-now local codex "$session_id" \
            reverse-redirect 'Reverse redirect storage'
)"
grep -Fqx 'storage_mode=remote:backup' <<< "$reverse_redirect_save"
reverse_record_id="$(sed -n 's/^record_id=//p' <<< "$reverse_redirect_save")"
reverse_record_path="$(sed -n 's/^path=//p' <<< "$reverse_redirect_save")"
reverse_record_digest="$(sha256sum "$reverse_record_path" | cut -d' ' -f1)"
reverse_record_marker="ags-v1/records/codex/$reverse_record_id.$reverse_record_digest.record"
git --git-dir="$tmp/git/records.git" cat-file -e \
    "main:$reverse_record_marker"

git_retire="$(env "${merge_env[@]}" "$tool" storage retire \
    final --into backup)"
grep -Fqx 'status=retired' <<< "$git_retire"
grep -Fqx 'source=remote:final' <<< "$git_retire"
git_retired_path="$(sed -n 's/^recoverable_replica=//p' <<< "$git_retire")"
[[ "$git_retired_path" == "$merge_final_git#main:.ags-retired/"*"/ags-v1" ]]
git_final_head="$(git --git-dir="$merge_final_git" rev-parse main)"
[[ "$(git --git-dir="$merge_final_git" rev-parse main^)" == \
   "$final_before_retire" ]]
git --git-dir="$merge_final_git" cat-file -e \
    'main:.ags-retired/ags-v1.retired'
if git --git-dir="$merge_final_git" cat-file -e 'main:ags-v1' 2>/dev/null; then
    echo 'Git retirement left the active AGS tree present' >&2
    exit 1
fi
git_retired_tree_path="$(
    git --git-dir="$merge_final_git" ls-tree -r --name-only "$git_final_head" |
        sed -n 's#^\(.ags-retired/[^/]*/ags-v1\)/.*#\1#p' |
        head -n 1
)"
[[ -n "$git_retired_tree_path" ]]
[[ "$(git --git-dir="$merge_final_git" rev-parse \
    "$git_final_head:$git_retired_tree_path")" == "$final_tree_before_retire" ]]
if env "${merge_env[@]}" "$tool" remote add final git \
    "$merge_final_git" --branch main >/dev/null 2>&1; then
    echo 'remote add resurrected a retired Git replica' >&2
    exit 1
fi

empty_git="$tmp/storage-merge/empty.git"
git init -q --bare "$empty_git"
env "${merge_env[@]}" "$tool" remote add empty git \
    "$empty_git" --branch main >/dev/null
empty_merge="$(env "${merge_env[@]}" "$tool" storage merge \
    --into backup empty)"
grep -Fqx 'source=remote:empty' <<< "$empty_merge"
empty_retire="$(env "${merge_env[@]}" "$tool" storage retire \
    empty --into backup)"
grep -Fqx 'status=retired' <<< "$empty_retire"
grep -Fqx "recoverable_replica=$empty_git#main:no-active-replica" \
    <<< "$empty_retire"
git --git-dir="$empty_git" cat-file -e 'main:.ags-retired/ags-v1.retired'
if git --git-dir="$empty_git" cat-file -e 'main:ags-v1' 2>/dev/null; then
    echo 'empty Git retirement created an active AGS tree' >&2
    exit 1
fi
if env "${merge_env[@]}" "$tool" remote add empty git \
    "$empty_git" --branch main >/dev/null 2>&1; then
    echo 'remote add resurrected an empty retired Git replica' >&2
    exit 1
fi

sftp_remove="$(env "${sftp_env[@]}" "$tool" remote remove sftp-backup)"
grep -Fqx 'status=removed' <<< "$sftp_remove"
! env "${sftp_env[@]}" "$tool" remote list | grep -Fq sftp-backup

# codext reports the upstream Codex version because it *is* that version, so a
# second build on one upstream base publishes under the same tag and the tag
# cannot say which of the two a host has. The asset digest is the discriminator,
# and these four runs are the whole contract: install, recognise the same bytes,
# notice new bytes under an unchanged tag, and refuse a download that does not
# match the digest it is about to be recorded as.
codext_root="$tmp/codext-update"
mkdir -p "$codext_root/bin" "$codext_root/state" "$codext_root/serve"
printf '%s\n' '#!/bin/sh' 'echo "codex-cli 0.146.0 installed"' \
    > "$codext_root/bin/codext"
chmod +x "$codext_root/bin/codext"

# One release, one tag, whatever bytes the marker produces. Every platform's
# asset name is listed so the test does not depend on the host it runs on.
codext_publish() {
    local marker="$1" stage digest
    stage="$(mktemp -d "$tmp/codext-stage.XXXXXX")"
    printf '%s\n' '#!/bin/sh' "echo \"codex-cli 0.146.0 $marker\"" \
        > "$stage/codext"
    chmod 755 "$stage/codext"
    tar -czf "$codext_root/serve/asset.tar.gz" -C "$stage" codext
    rm -rf -- "$stage"
    digest="$(sha256sum "$codext_root/serve/asset.tar.gz" | cut -d' ' -f1)"
    jq -n --arg digest "sha256:${2:-$digest}" '{
      tag_name: "v0.146.0",
      assets: ([
        "codext-x86_64-unknown-linux-musl.tar.gz",
        "codext-x86_64-unknown-linux-gnu.tar.gz",
        "codext-aarch64-unknown-linux-musl.tar.gz",
        "codext-aarch64-unknown-linux-gnu.tar.gz",
        "codext-aarch64-apple-darwin.tar.gz",
        "codext-x86_64-apple-darwin.tar.gz"
      ] | to_entries | map({name: .value, id: (1000 + .key), digest: $digest}))
    }' > "$codext_root/serve/release.json"
}

cat > "$codext_root/bin/curl" <<'CODEXTCURL'
#!/usr/bin/env bash
# Serves the canned codext release. The metadata call reads stdout and appends
# the status code `-w` asked for; the asset call writes to the path after -o.
out=
url=
prev=
for arg in "$@"; do
    [[ "$prev" != -o ]] || out="$arg"
    [[ "$arg" != https://* ]] || url="$arg"
    prev="$arg"
done
case "$url" in
    */releases/latest)
        cat "$CODEXT_FAKE_SERVE/release.json"
        printf '\n200'
        ;;
    */releases/assets/*)
        cp -- "$CODEXT_FAKE_SERVE/asset.tar.gz" "$out"
        ;;
    *codex-package-*)
        # 只有备好了整包才供应它。没备的时候走 404，正是"上游包拿不到"那条
        # 退路——上面那几轮断言的就是它。
        [ -f "$CODEXT_FAKE_SERVE/package.tar.gz" ] || exit 22
        cp -- "$CODEXT_FAKE_SERVE/package.tar.gz" "$out"
        ;;
    *)
        exit 1
        ;;
esac
CODEXTCURL
chmod +x "$codext_root/bin/curl"

codext_env=(
    HOME="$tmp/home"
    PATH="$codext_root/bin:/usr/local/bin:/usr/bin:/bin"
    AGENT_SESSION_STATE_DIR="$codext_root/state"
    CODEXT_FAKE_SERVE="$codext_root/serve"
    CODEXT_UPDATE_TOKEN=fake-token
)
codext_stamp="$codext_root/state/codext-release"

codext_publish first
env "${codext_env[@]}" "$tool" codext-update > "$tmp/codext-1.out" 2>&1
grep -Fq 'updating codext to 0.146.0' "$tmp/codext-1.out"
grep -Fq 'updated codext to 0.146.0' "$tmp/codext-1.out"
grep -Fq '0.146.0 first' <<< "$("$codext_root/bin/codext")"
grep -Fqx "sha256:$(sha256sum "$codext_root/serve/asset.tar.gz" | cut -d' ' -f1)" \
    "$codext_stamp"

# Same bytes, same tag: nothing to do. Before the digest replaced the tag
# comparison this said "updating 0.146.0 -> v0.146.0-2" on every single run.
env "${codext_env[@]}" "$tool" codext-update > "$tmp/codext-2.out" 2>&1
grep -Fq 'already current: codext 0.146.0' "$tmp/codext-2.out"
! grep -Fq 'updated codext' "$tmp/codext-2.out"

# New bytes under the unchanged tag: the case a tag comparison cannot see.
codext_publish second
env "${codext_env[@]}" "$tool" codext-update > "$tmp/codext-3.out" 2>&1
grep -Fq 'updated codext to 0.146.0' "$tmp/codext-3.out"
! grep -Fq 'already current' "$tmp/codext-3.out"
grep -Fq '0.146.0 second' <<< "$("$codext_root/bin/codext")"
codext_installed_digest="$(cat "$codext_stamp")"

# A download that does not hash to the published digest is not installed, and
# the stamp keeps naming the build that is actually on disk rather than the one
# that failed to arrive.
codext_publish third \
    0000000000000000000000000000000000000000000000000000000000000000
env "${codext_env[@]}" "$tool" codext-update > "$tmp/codext-4.out" 2>&1
grep -Fq '对不上' "$tmp/codext-4.out"
grep -Fq '0.146.0 second' <<< "$("$codext_root/bin/codext")"
grep -Fqx "$codext_installed_digest" "$codext_stamp"

# codext = 上游整包 + 换掉一个 entrypoint。
#
# 上面四轮走的是"上游包拿不到"那条退路（假 curl 对 codex-package 返回 22），装出来
# 的是光杆二进制。这一轮备好整包，断言的是应有的形态：树整个来自上游，只有清单
# 指的那个 entrypoint 是我们的构建，而 PATH 上的 `codext` 是指进树里的软链接。
#
# 这条测试真正盯住的是"以后上游往包里加东西，我们自动带上"：sidecar 不是我们枚举
# 出来的，是包里有什么就装什么。所以这里放一个我们代码里从没提过名字的文件
# （`bin/codex-some-future-sidecar`），它必须也被装进去。
codext_pkg_stage="$(mktemp -d "$tmp/codext-pkg.XXXXXX")"
mkdir -p "$codext_pkg_stage/bin" "$codext_pkg_stage/codex-resources"
printf '%s\n' '#!/bin/sh' 'echo upstream-entrypoint' > "$codext_pkg_stage/bin/codex"
printf '%s\n' '#!/bin/sh' 'echo upstream-host' \
    > "$codext_pkg_stage/bin/codex-code-mode-host"
printf '%s\n' '#!/bin/sh' 'echo future' \
    > "$codext_pkg_stage/bin/codex-some-future-sidecar"
printf 'upstream-resource\n' > "$codext_pkg_stage/codex-resources/marker"
chmod 755 "$codext_pkg_stage/bin/"*
jq -n '{
  layoutVersion: 1,
  version: "0.146.0",
  target: "test",
  variant: "codex",
  entrypoint: "bin/codex",
  resourcesDir: "codex-resources",
  pathDir: "codex-path"
}' > "$codext_pkg_stage/codex-package.json"
tar -czf "$codext_root/serve/package.tar.gz" -C "$codext_pkg_stage" .
rm -rf -- "$codext_pkg_stage"

codext_publish fifth
codext_pkg_root="$codext_root/pkgroot"
env "${codext_env[@]}" AGS_CODEXT_PACKAGE_ROOT="$codext_pkg_root" \
    "$tool" codext-update > "$tmp/codext-5.out" 2>&1
grep -Fq '按上游整包布局装在' "$tmp/codext-5.out"
# entrypoint 是我们的构建，不是包里那个占位的。
grep -Fq '0.146.0 fifth' <<< "$("$codext_pkg_root/0.146.0/bin/codex")"
# 其余全部是上游的，包括我们代码里从没写过名字的那个。
grep -Fqx upstream-host <<< "$("$codext_pkg_root/0.146.0/bin/codex-code-mode-host")"
grep -Fqx future <<< "$("$codext_pkg_root/0.146.0/bin/codex-some-future-sidecar")"
grep -Fqx upstream-resource < "$codext_pkg_root/0.146.0/codex-resources/marker"
# PATH 上那个名字变成指进树里的软链接，跑起来仍然是我们的构建。
[[ -L "$codext_root/bin/codext" ]]
grep -Fq '0.146.0 fifth' <<< "$("$codext_root/bin/codext")"

printf 'ags self-check passed\n'
