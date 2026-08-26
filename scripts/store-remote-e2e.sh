#!/usr/bin/env bash
# `ags store` 的真实远端端到端：GitHub（git 后端）+ Vultr Sydney（sftp 后端）。
#
# 全程沙箱：自己的 HOME / CODEX_HOME / CLAUDE_CONFIG_DIR / AGENT_SESSION_STATE_DIR。
# 这台机器上真实的 ~/.codex、~/.claude、~/.local/share/ags 一个字节都不碰。
#
# 跨机器那一半是真的跨沙箱：第二个沙箱有**自己的** HOME 和状态目录，只共用同一份
# age 身份（记录是加密的，换身份就读不出来——那正是该验的东西）。
set -uo pipefail

BIN="${AGS_TEST_BINARY:-${CARGO_TARGET_DIR:-$PWD/target}/debug/ags}"
STAMP=$(date +%s)
REPO="ags-store-test-$STAMP"
REPO_FULL="jk-zhang-meta/$REPO"
# 服务器和密码从环境里来，不进仓库。跑法：
#
#   AGS_E2E_SFTP_HOST=… AGS_E2E_SFTP_USER=root \
#   AGENT_SESSION_REMOTE_PASSWORD=… bash scripts/store-remote-e2e.sh
#
# GitHub 那半用本机的 `gh auth token`。
SFTP_HOST="${AGS_E2E_SFTP_HOST:-}"
SFTP_USER="${AGS_E2E_SFTP_USER:-root}"
SFTP_PATH="/root/ags-store-test-$STAMP"
[[ -n "$SFTP_HOST" && -n "${AGENT_SESSION_REMOTE_PASSWORD:-}" ]] || {
    echo "需要 AGS_E2E_SFTP_HOST 和 AGENT_SESSION_REMOTE_PASSWORD" >&2
    exit 1
}
export AGENT_SESSION_REMOTE_PASSWORD

GH_TOKEN="$(gh auth token 2>/dev/null)"
[[ -n "$GH_TOKEN" ]] || { echo "拿不到 GitHub token（gh auth token）"; exit 1; }
tmp=$(mktemp -d /tmp/store-remote.XXXXXX)
pass=0; fail=0
ok()   { pass=$((pass+1)); printf '  ✓ %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf '  ✗ %s\n%s\n' "$1" "$(sed 's/^/      /' <<<"${2:-}" | head -6)"; }
check(){ local what="$1" needle="$2" out="$3"
         if grep -Fq -- "$needle" <<<"$out"; then ok "$what"; else bad "$what" "$out"; fi; }

cleanup() {
    echo
    echo "== 清理"
    gh repo delete "$REPO_FULL" --yes >/dev/null 2>&1 &&
        echo "  GitHub 仓库已删除: $REPO_FULL" || echo "  GitHub 仓库删除失败（可能没建成）"
    timeout 30 sshpass -p "$AGENT_SESSION_REMOTE_PASSWORD" ssh \
        -o StrictHostKeyChecking=no -o ConnectTimeout=12 "$SFTP_USER@$SFTP_HOST" \
        "rm -rf $SFTP_PATH" >/dev/null 2>&1 &&
        echo "  远端目录已删除: $SFTP_PATH" || echo "  远端目录删除失败"
    rm -rf -- "$tmp"
    echo "  沙箱已删除"
}
trap cleanup EXIT

# ---------------------------------------------------------------- 沙箱工厂
# 一个沙箱＝一台"机器"。身份文件传进来才共用，不传就自己生成。
box_env() {   # box_env 名字  → 打印可 eval 的 env 前缀
    # 分两行：同一条 `local` 里 `home="$tmp/$name"` 取到的是那个还没赋值的局部
    # 变量，`set -u` 下当场 unbound variable。
    local name="$1" home
    home="$tmp/$name"
    printf 'HOME=%s CODEX_HOME=%s CLAUDE_CONFIG_DIR=%s ' "$home" "$home/codex" "$home/claude"
    printf 'XDG_CONFIG_HOME=%s XDG_DATA_HOME=%s XDG_STATE_HOME=%s ' \
        "$home/.config" "$home/.local/share" "$home/.local/state"
    printf 'AGENT_SESSION_STATE_DIR=%s PATH=%s ' "$tmp/$name-state" \
        "$tmp/$name-bin:/usr/local/bin:/usr/bin:/bin"
    printf 'AGS_UPDATE_CHECK=0 AGS_AUTO_GC=0 AGS_AUTO_SUMMARY=0'
}
mk() {   # mk 名字
    local name="$1" home
    home="$tmp/$name"
    mkdir -p "$home/codex/sessions/2026/01/01" "$home/claude" "$tmp/$name-state" "$tmp/$name-bin"
    printf '%s\n' '#!/bin/sh' 'echo FAKE_CODEX' > "$tmp/$name-bin/codex"
    chmod 0755 "$tmp/$name-bin/codex"
    # 沙箱换掉了 HOME，git 于是找不到这台机器的凭据，报的是
    # `could not read Username for https://github.com`。而 ags 按设计**拒绝 URL 里
    # 嵌密码**，所以只能像真机那样配一个 credential helper。
    printf '[credential]\n\thelper = store\n[user]\n\tname = ags e2e\n\temail = ags@example.invalid\n' \
        > "$home/.gitconfig"
    printf 'https://x-access-token:%s@github.com\n' "$GH_TOKEN" > "$home/.git-credentials"
    chmod 0600 "$home/.git-credentials"
}
A() { eval env $(box_env boxA) "$BIN" checkpoint "$@"; }
B() { eval env $(box_env boxB) "$BIN" checkpoint "$@"; }

# ---------------------------------------------------------------- 准备
echo "== 准备两个沙箱"
mk boxA; mk boxB

SESSION_ID=abcdabcd-1111-4111-8111-111111111111
ROLLOUT="$tmp/boxA/codex/sessions/2026/01/01/rollout-e2e-$SESSION_ID.jsonl"
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
printf '%s\n' \
  "{\"timestamp\":\"$TS\",\"type\":\"session_meta\",\"payload\":{\"id\":\"$SESSION_ID\",\"cwd\":\"$tmp\",\"model_provider\":\"openai\"}}" \
  "{\"timestamp\":\"$TS\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"store 端到端测试用的会话\"}}" \
  > "$ROLLOUT"

out=$(A init 2>&1); check "boxA 初始化" "status=initialized" "$out"
IDENTITY="$tmp/boxA/.config/ags/identity.agekey"
[[ -f "$IDENTITY" ]] && ok "boxA 生成了身份" || bad "boxA 没有身份"

# boxB 用同一份身份——跨机器共享记录靠的就是这个。
mkdir -p "$tmp/boxB/.config/ags"
out=$(B init --identity "$IDENTITY" 2>&1); check "boxB 用同一份身份初始化" "status=initialized" "$out"

out=$(A save --session "codex/$SESSION_ID" '端到端：store 统一' 2>&1)
check "boxA 存了一份存档" "description=" "$out"
RECORD_ID=$(sed -n 's/^record_id=//p' <<<"$out" | head -1)
[[ -n "$RECORD_ID" ]] && ok "拿到 record_id: $RECORD_ID" || bad "没拿到 record_id" "$out"

# ---------------------------------------------------------------- A. GitHub
echo
echo "== A. GitHub（git 后端）"
if gh repo create "$REPO_FULL" --private --description 'ags store e2e (throwaway)' >/dev/null 2>&1; then
    ok "建了私有仓库 $REPO_FULL"
else
    bad "建仓库失败"
fi
GIT_URL="https://github.com/$REPO_FULL.git"

out=$(A store add gh git "$GIT_URL" 2>&1); check "store add gh git URL" "type=git" "$out"
out=$(A store 2>&1);       check "列表里出现 gh" "gh" "$out"
out=$(A store 2>&1);       check "类型标成 git" "git" "$out"
out=$(A store show gh 2>&1); check "show 报出 URL" "$REPO" "$out"
out=$(A store show gh 2>&1); check "store show gh" "git" "$out"

out=$(A store use gh 2>&1); check "切到 gh" "selected" "$out"
out=$(A sync --dry-run 2>&1); check "sync --dry-run 出计划" "" "$out"
echo "      (计划: $(head -3 <<<"$out" | tr '\n' ' '))"
out=$(A sync 2>&1); check "sync 推上去" "" "$out"
echo "      (sync: $(tail -3 <<<"$out" | tr '\n' ' '))"

# 仓库里真的有东西了吗——问 GitHub，不是问本地。
files=$(gh api "repos/$REPO_FULL/git/trees/HEAD?recursive=1" \
        --jq '[.tree[]?.path] | length' 2>/dev/null || printf 0)
[[ "$files" =~ ^[0-9]+$ ]] || files=0
if (( files > 0 )); then
    ok "GitHub 上有 $files 个对象"
    gh api "repos/$REPO_FULL/git/trees/HEAD?recursive=1" --jq '.tree[].path' 2>/dev/null |
        head -5 | sed 's/^/      /'
else
    bad "GitHub 上什么都没有"
fi

echo "  -- boxB 从 GitHub 拉"
out=$(B store add gh git "$GIT_URL" 2>&1); check "boxB 加同一个 store" "type=git" "$out"
out=$(B store use gh 2>&1);                check "boxB 切过去" "selected" "$out"
out=$(B sync 2>&1);                        check "boxB sync" "" "$out"
echo "      (sync: $(tail -3 <<<"$out" | tr '\n' ' '))"
out=$(B archives 2>&1)
check "boxB 看得见 boxA 存的那份" "端到端：store 统一" "$out"

# ---------------------------------------------------------------- B. SFTP
echo
echo "== B. Vultr Sydney（sftp 后端）"
KNOWN="$tmp/known_hosts"
ssh-keyscan -T 15 -p 22 "$SFTP_HOST" > "$KNOWN" 2>/dev/null
[[ -s "$KNOWN" ]] && ok "抓到 host key" || bad "抓不到 host key"
timeout 30 sshpass -p "$AGENT_SESSION_REMOTE_PASSWORD" ssh \
    -o StrictHostKeyChecking=no -o ConnectTimeout=12 "$SFTP_USER@$SFTP_HOST" \
    "mkdir -p $SFTP_PATH" >/dev/null 2>&1 && ok "远端建好目录" || bad "远端建目录失败"

SFTP_URL="sftp://$SFTP_USER@$SFTP_HOST:22$SFTP_PATH"
out=$(A store add syd "$SFTP_URL" --known-hosts "$KNOWN" --password 2>&1)
check "store add syd sftp://…" "type=sftp" "$out"
out=$(A store 2>&1);         check "列表里 gh 和 syd 都在" "syd" "$out"
out=$(A store show syd 2>&1); check "store show syd 报 sftp" "sftp" "$out"

out=$(A store use syd 2>&1); check "切到 syd" "selected" "$out"
out=$(A sync 2>&1);          check "往 sftp 推" "" "$out"
echo "      (sync: $(tail -3 <<<"$out" | tr '\n' ' '))"

remote_ls=$(timeout 30 sshpass -p "$AGENT_SESSION_REMOTE_PASSWORD" ssh \
    -o StrictHostKeyChecking=no -o ConnectTimeout=12 "$SFTP_USER@$SFTP_HOST" \
    "find $SFTP_PATH -type f | head -8" 2>/dev/null)
if [[ -n "$remote_ls" ]]; then
    ok "服务器上真的落了文件"
    sed 's/^/      /' <<<"$remote_ls" | head -5
else
    bad "服务器上什么都没有"
fi

echo "  -- boxB 从 sftp 拉"
out=$(B store add syd "$SFTP_URL" --known-hosts "$KNOWN" --password 2>&1)
check "boxB 加 sftp store" "type=sftp" "$out"
out=$(B store use syd 2>&1); check "boxB 切到 syd" "selected" "$out"
out=$(B sync 2>&1);          check "boxB 从 sftp 同步" "" "$out"
out=$(B archives 2>&1);      check "boxB 从 sftp 也看得见那份存档" "端到端：store 统一" "$out"

# ---------------------------------------------------------------- C. 管理动作
echo
echo "== C. store 管理"
out=$(A store use local 2>&1);  check "切回 local" "selected" "$out"
out=$(A store remove syd 2>&1); check "删掉 syd" "" "$out"
out=$(A store 2>&1)
if grep -Fq syd <<<"$out"; then bad "syd 删了还在列表里" "$out"; else ok "syd 不在列表里了"; fi
if grep -Fq gh  <<<"$out"; then ok "gh 还在"; else bad "gh 不见了" "$out"; fi
out=$(A store remove local 2>&1 || true)
check "拒绝删 local" "ags store add local" "$out"

echo
printf '真实远端端到端：通过 %s，失败 %s\n' "$pass" "$fail"
[[ "$fail" == 0 ]]
