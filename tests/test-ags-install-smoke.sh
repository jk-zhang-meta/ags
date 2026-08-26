#!/usr/bin/env bash
set -Eeuo pipefail

# 失败时说出是哪一行。
#
# 这个套件全是裸断言（`[[ -x … ]]`、`grep -Fq …`），配上 `set -e` 的结果是失败时
# **一个字都不打**：CI 上只有一行 `Process completed with exit code 1`，安装器本身
# 又是 `--quiet` 的，等于什么线索都没有。`-E` 不能少，否则 trap 进不了函数和子 shell。
#
# 记录到文件、真死了才由 EXIT 打出来：套件里有故意失败的用例（离线守卫那几处），
# 当场打印会让一次全绿的运行里冒出看着像失败的行。文件而不是变量，是因为不少断言
# 在子 shell 里，改不到父进程的变量。后写覆盖先写，留下的是最后一次。
smoke_errfile="$(mktemp "${TMPDIR:-/tmp}/ags-smoke-err.XXXXXX")"
trap 'printf "第 %s 行（退出码 %s）：%s\n" "$LINENO" "$?" "$BASH_COMMAND" \
    > "$smoke_errfile" 2>/dev/null || true' ERR

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo_target="${CARGO_TARGET_DIR:-$project_root/target}"
binary="${AGS_TEST_BINARY:-$cargo_target/debug/ags}"

# 把继承来的 AGS_*/AGENT_SESSION_* 擦干净。
#
# 从一个 ags 起来的 shell 里跑这个套件，`AGS_ID` 会跟着进来，于是
# `ags save installer-hook` 存出来的检查点用的是**外面那个会话的 ID**，断言找不到
# 自己刚存的东西，报的却是「检查点不存在」——跟真实原因隔着十万八千里。
# 套件要用的每个变量下面都用 `env` 显式给，全擦是安全的。
while IFS='=' read -r ags_inherited _; do
    case "$ags_inherited" in
        AGS_*|AGENT_SESSION_*) unset "$ags_inherited" ;;
    esac
done < <(env)
unset ags_inherited
# 装的是哪个版本，问被测二进制自己。
#
# 这里原来写死 `v0.3.0-ags.1`，而安装器会拿它和产物的真实版本比对——也就是说
# 0.3.0 之后每一次发版都会让这个套件失败。它一直没被发现，是因为在 CI 里它排在
# 运行时套件后面，而运行时套件先红，这一条根本没机会跑。
smoke_version="v$("$binary" --version | awk '{print $2}')"
platform="$(uname -s)"
test_tmp_root=/tmp
[[ "$platform" != Darwin ]] || test_tmp_root=/private/tmp
tmp="$(mktemp -d "$test_tmp_root/ags-install-smoke.XXXXXX")"
export FAKE_REAL_NODE_BINARY="$(command -v node)"
[[ -x "$FAKE_REAL_NODE_BINARY" ]] || {
    printf 'node is required (the offline guard shadows it with a stub)\n' >&2
    exit 1
}

cleanup() {
    local status=$?
    if (( status != 0 )) && [[ -s "${smoke_errfile:-}" ]]; then
        printf 'ags install smoke: %s' "$(cat "$smoke_errfile")" >&2
    fi
    rm -f "${smoke_errfile:-}"
    case "$tmp" in
        /tmp/ags-install-smoke.*|/private/tmp/ags-install-smoke.*) rm -rf -- "$tmp" ;;
    esac
}
trap cleanup EXIT

[[ -x "$binary" ]] || {
    printf 'ags test binary is missing: %s\n' "$binary" >&2
    exit 1
}

test_sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -- "$1" | awk '{print $1}'
    else
        shasum -a 256 -- "$1" | awk '{print $1}'
    fi
}


artifact_root="$tmp/artifact"
artifact="$tmp/ags.tar.xz"
home="$tmp/home"
bin_dir="$tmp/bin"
offline_guard_bin="$tmp/offline-guard-bin"
offline_network_marker="$tmp/offline-network-used"
mkdir -p "$artifact_root" "$home/.codex" "$home/.claude" "$offline_guard_bin"
ln -s "$FAKE_REAL_NODE_BINARY" "$offline_guard_bin/node-real"
install -m 0755 "$binary" "$artifact_root/ags"
tar -cJf "$artifact" -C "$artifact_root" ags

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
real_node="${FAKE_REAL_NODE_BINARY:-${BASH_SOURCE[0]%/*}/node-real}"
exec "$real_node" "$script" "$@"
EOF
chmod +x "$offline_guard_bin/node"
cat > "$offline_guard_bin/claude" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'FAKE_CLAUDE %s\n' "$*"
EOF
chmod +x "$offline_guard_bin/claude"
cat > "$offline_guard_bin/codex" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'FAKE_CODEX %s\n' "$*"
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
        OFFLINE_NETWORK_MARKER="$offline_network_marker" \
        VERSION="$smoke_version" \
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
# 二进制自己就叫 ags 了，旁边不再有壳，装完也不该留下旧的 casr。
[[ -x "$bin_dir/ags" && ! -e "$bin_dir/casr" ]]
[[ -f "$identity" && -f "$config" && -d "$vault" ]]
[[ "$(stat -c %a "$identity")" == 600 ]]
[[ "$(stat -c %a "$config")" == 600 ]]
jq -e --arg vault "$vault" --arg identity "$identity" '
    .version == 4 and .local_path == $vault and
    .encryption == {type:"age-x25519", identity_file:$identity}
' "$config" >/dev/null

for root in "$home/.codex/skills" "$home/.claude/skills"; do
    cmp "$project_root/plugins/ags/skills/ags/SKILL.md" "$root/ags/SKILL.md"
    cmp "$project_root/plugins/ags/skills/ags/agents/openai.yaml" \
        "$root/ags/agents/openai.yaml"
done
hook_command="$bin_dir/ags hook"
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

# 假的 codex：`ags save` 会先把 Agent 解析出来，解析不到就
# `cannot find an executable codex binary`。跑步机上没装 Codex，所以这一段本来
# **只在装了 Codex 的开发机上是绿的**——这个套件排在运行时套件后面、那个一直红，
# 所以从来没人看见。套件要自带被依赖的东西，不能指望宿主有。
agent_stub_bin="$tmp/agent-stub-bin"
mkdir -p "$agent_stub_bin"
printf '%s\n' '#!/bin/sh' 'exit 0' > "$agent_stub_bin/codex"
chmod 0755 "$agent_stub_bin/codex"

runtime_env=(
    env
    HOME="$home"
    XDG_CONFIG_HOME="$home/.config"
    XDG_DATA_HOME="$home/.local/share"
    XDG_STATE_HOME="$home/.local/state"
    PATH="$bin_dir:$agent_stub_bin:$PATH"
)
# 裸命名空间归运行时，`convert` 下面才是转换 CLI，两边各验一句。
"${runtime_env[@]}" "$bin_dir/ags" --help | grep -Fq '用法:'
"${runtime_env[@]}" "$bin_dir/ags" convert --help | grep -Fq 'Usage:'
"${runtime_env[@]}" "$bin_dir/ags" archives >/dev/null

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
        "$bin_dir/ags" hook
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
interrupted_home="$tmp/interrupted-home"
interrupted_bin="$tmp/interrupted-bin"
interrupted_backup="$interrupted_bin/.ags.rollback.recovery"
interrupted_stage="$interrupted_bin/.ags.install.recovery"
interrupted_journal="$interrupted_bin/.ags.install-transaction.json"
interrupted_missing_artifact="$tmp/interrupted-missing.tar.xz"
mkdir -p "$interrupted_home" "$interrupted_bin"
printf '%s\n' '#!/bin/sh' 'printf "recovered-ags\n"' \
    > "$interrupted_backup"
chmod 755 "$interrupted_backup"
interrupted_previous_sha="$(test_sha256_file "$interrupted_backup")"
interrupted_candidate_sha="$(test_sha256_file "$binary")"
jq -n \
    --arg binary "$interrupted_bin/ags" \
    --arg candidate_sha "$interrupted_candidate_sha" \
    --arg stage "$interrupted_stage" \
    --arg previous_sha "$interrupted_previous_sha" \
    --arg backup "$interrupted_backup" \
    '{
      schema:2,
      managed_by:"ags-installer",
      binary_path:$binary,
      candidate:{sha256:$candidate_sha,stage_path:$stage},
      previous:{
        existed:true,
        sha256:$previous_sha,
        backup_path:$backup
      },
    }' > "$interrupted_journal"
chmod 600 "$interrupted_journal"
if env HOME="$interrupted_home" \
    XDG_CONFIG_HOME="$interrupted_home/.config" \
    XDG_DATA_HOME="$interrupted_home/.local/share" \
    XDG_STATE_HOME="$interrupted_home/.local/state" \
    PATH="$offline_guard_bin:$interrupted_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION="$smoke_version" \
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
[[ "$("$interrupted_bin/ags")" == recovered-ags ]]
[[ "$interrupted_previous_sha" == "$(
    test_sha256_file "$interrupted_bin/ags"
)" ]]
[[ ! -e "$interrupted_journal" && ! -e "$interrupted_backup" ]]

# 装到一个已经有 `ags` 的目录上：要覆盖，不要保留。
#
# 这一条原来断言的是反过来的——那时 `ags` 是 `casr` 旁边的一个壳脚本，安装器认
# `# casr-installer-wrapper` 标记，认不出来就当"手写的"留着。壳没了之后 `ags` 就是
# 二进制本身，而 casr→ags 迁移**正是靠覆盖旧的 `ags` 壳**完成的（实测：144 字节的壳
# → 4,504,688 字节的 Mach-O）。保留反而会让升级过的机器永远停在旧壳上。
migrate_home="$tmp/migrate-home"
migrate_bin="$tmp/migrate-bin"
mkdir -p "$migrate_home" "$migrate_bin"
printf 'unmanaged\n' > "$migrate_bin/ags"
env HOME="$migrate_home" \
    XDG_CONFIG_HOME="$migrate_home/.config" \
    XDG_DATA_HOME="$migrate_home/.local/share" \
    XDG_STATE_HOME="$migrate_home/.local/state" \
    PATH="$offline_guard_bin:$migrate_bin:$PATH" \
    OFFLINE_NETWORK_MARKER="$offline_network_marker" \
    VERSION="$smoke_version" \
    "$project_root/install.sh" --offline "$artifact" --dest "$migrate_bin" \
    --no-verify --quiet > "$tmp/migrate.out" 2> "$tmp/migrate.err"
! grep -Fqx 'unmanaged' "$migrate_bin/ags"
[[ -x "$migrate_bin/ags" ]]
"$migrate_bin/ags" --version >/dev/null

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
    VERSION="$smoke_version" \
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

if (( EUID == 0 )); then
    if env HOME="$tmp/system-root-home" \
        XDG_CONFIG_HOME="$tmp/system-root-home/.config" \
        XDG_DATA_HOME="$tmp/system-root-home/.local/share" \
        XDG_STATE_HOME="$tmp/system-root-home/.local/state" \
        "$project_root/install.sh" --system --offline "$artifact" \
        --no-verify --quiet > "$tmp/system-root.out" \
        2> "$tmp/system-root.err"; then
        printf 'installer accepted root --system for a per-user setup\n' >&2
        exit 1
    fi
    grep -Fq -- '--system cannot run as root' "$tmp/system-root.err"
fi

# 改名之前 ags 是个四行的壳，真正的二进制叫 casr。二进制现在自己就叫 ags，
# 也就是说新二进制要落的位置，正是那个壳待着的位置——每一台装过旧版的机器都长这样，
# 所以这条路必须能走通：壳被换成二进制，旧的 casr 被收走。
#
# 能覆盖的前提是目标得是个普通文件（见 install.sh 里那道 non-regular 闸门）。壳是
# 普通文件，所以会被备份后替换。
cat > "$bin_dir/ags" <<'LEGACY_WRAPPER'
#!/bin/sh
# ags-installer-checkpoint-wrapper
exec "$(dirname -- "$0")/casr" checkpoint "$@"
LEGACY_WRAPPER
chmod 0755 "$bin_dir/ags"
install -m 0755 "$binary" "$bin_dir/casr"
if ! run_installer > "$tmp/legacy-wrapper.out" 2> "$tmp/legacy-wrapper.err"; then
    printf 'installer run over a pre-rename wrapper failed:\n' >&2
    cat "$tmp/legacy-wrapper.err" >&2
    exit 1
fi
if grep -Fq installer-checkpoint-wrapper "$bin_dir/ags"; then
    printf 'installer left the old wrapper in place instead of replacing it\n' >&2
    exit 1
fi
"$bin_dir/ags" --version | grep -Fq ags
if [ -e "$bin_dir/casr" ]; then
    printf 'installer left the old casr binary behind\n' >&2
    exit 1
fi

printf 'ags install smoke passed (%s/%s)\n' "$platform" "$(uname -m)"
