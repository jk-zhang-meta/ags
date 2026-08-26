#!/usr/bin/env bash
#
# ags installer
#
# One-liner install (with cache buster):
#   curl -fsSL "https://raw.githubusercontent.com/jk-zhang-meta/ags/main/install.sh?$(date +%s)" | bash
#
# Or without cache buster:
#   curl -fsSL https://raw.githubusercontent.com/jk-zhang-meta/ags/main/install.sh | bash
#
# Options:
#   --version vX.Y.Z   Install specific version (default: latest)
#   --dest DIR         Install to DIR (default: ~/.local/bin)
#   --system           Install to user-writable /usr/local/bin (never with sudo)
#   --easy-mode        Auto-update PATH in shell rc files
#   --yes              Non-interactive; auto-accept install prompts
#   --verify           Run self-test after install
#   --from-source      Build from source instead of downloading binary
#   --quiet            Suppress non-error output
#   --no-gum           Disable gum formatting even if available
#   --no-verify        Skip checksum + signature verification (not recommended)
#   --no-configure     Skip optional agent setup
#   --no-skill         Skip skill installation for Claude/Codex
#   --offline TARBALL  Install from local tarball (airgap mode)
#   --force            Reinstall even if same version exists
#
set -euo pipefail
umask 022
shopt -s lastpipe 2>/dev/null || true

# Require bash >= 4.4 for safe empty-array expansion with set -u
if [[ "${BASH_VERSINFO[0]}" -lt 4 || ( "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -lt 4 ) ]]; then
  echo "Error: This installer requires bash >= 4.4 (yours is ${BASH_VERSION})." >&2
  echo "On macOS, install modern bash: brew install bash" >&2
  exit 1
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Configuration
# ═══════════════════════════════════════════════════════════════════════════════

VERSION="${VERSION:-}"
OWNER="${OWNER:-jk-zhang-meta}"
REPO="${REPO:-ags}"
BINARY_NAME="ags"

# 装的过程里不要让运行时自己去查更新。
#
# 它每天会去 GitHub 问一次有没有新版本。装到一半时那个问题没有意义——版本这会儿正在
# 被决定——而在 `--offline` 下它是**实打实的联网**，正是 `--offline` 承诺不会发生的
# 事。设在这里而不是逐个调用点：安装器会用好几种方式碰到运行时（init、钩子、技能、
# codext），漏一个就等于没设。
export AGS_UPDATE_CHECK=0
DEST_DEFAULT="$HOME/.local/bin"
DEST="${DEST:-$DEST_DEFAULT}"
SYSTEM_INSTALL=0
CLAUDE_CONFIG_ROOT="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
CODEX_CONFIG_ROOT="${CODEX_HOME:-$HOME/.codex}"
EASY=0
ASSUME_YES=0
QUIET=0
VERIFY=0
FROM_SOURCE=0
CHECKSUM="${CHECKSUM:-}"
CHECKSUM_URL="${CHECKSUM_URL:-}"
SIGSTORE_BUNDLE_URL="${SIGSTORE_BUNDLE_URL:-}"
COSIGN_IDENTITY_RE="${COSIGN_IDENTITY_RE:-^https://github.com/${OWNER}/${REPO}/.github/workflows/dist.yml@refs/tags/.*$}"
COSIGN_OIDC_ISSUER="${COSIGN_OIDC_ISSUER:-https://token.actions.githubusercontent.com}"
ARTIFACT_URL="${ARTIFACT_URL:-}"
LOCK_FILE="/tmp/ags-install.lock"
NO_GUM=0
NO_CHECKSUM=0
NO_CONFIGURE=0
NO_SKILL=0
FORCE_INSTALL=0
OFFLINE_TARBALL=""
CHECKPOINT_IDENTITY=""
CODEXT_STATUS="not-attempted"
CODEXT_RELEASE_STAMP="${XDG_STATE_HOME:-$HOME/.local/state}/ags/codext-release"
INSTALL_TRANSACTION_FILE=""
INSTALL_TRANSACTION_ACTIVE=0
INSTALL_CORE_COMMITTED=0
RESUME_CORE_ONLY=0
BINARY_PREEXISTED=0
BINARY_BACKUP=""
BINARY_STAGE=""
PROVIDER_VERSION_TIMEOUT="${AGS_INSTALLER_PROVIDER_VERSION_TIMEOUT:-1}"
SKILL_ARCHIVE_STATUS="not-attempted"
CLAUDE_SKILL_STATUS="not-detected"
CODEX_SKILL_STATUS="not-detected"
CC_WRAPPER_STATUS="not-attempted"
COD_WRAPPER_STATUS="not-attempted"
GMI_WRAPPER_STATUS="not-attempted"
AGS_WRAPPER_STATUS="not-attempted"
AGS_CODEX_SKILL_STATUS="not-attempted"
AGS_CLAUDE_SKILL_STATUS="not-attempted"
AGS_HOOK_STATUS="not-attempted"
AGS_INIT_STATUS="not-attempted"

# ═══════════════════════════════════════════════════════════════════════════════
# Output System (Gum + ANSI Dual-Path)
# ═══════════════════════════════════════════════════════════════════════════════

HAS_GUM=0
if command -v gum &>/dev/null && [ -t 1 ]; then
  HAS_GUM=1
fi

log() { [ "$QUIET" -eq 1 ] && return 0; echo -e "$@"; }

info() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 39 "→ $*"
  else
    echo -e "\033[0;34m→\033[0m $*"
  fi
}

ok() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 42 "✓ $*"
  else
    echo -e "\033[0;32m✓\033[0m $*"
  fi
}

warn() {
  [ "$QUIET" -eq 1 ] && return 0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 214 "⚠ $*"
  else
    echo -e "\033[1;33m⚠\033[0m $*"
  fi
}

err() {
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 196 "✗ $*" >&2
  else
    echo -e "\033[0;31m✗\033[0m $*" >&2
  fi
}

# 一句 `--quiet` 也压不住的警告，写 stderr。
#
# `warn` 在 `--quiet` 下直接 return，而且写的是 stdout。用它报"检查点没初始化"的
# 后果是：一次 `--quiet` 安装会**完全无声地**跳过整个检查点功能——装完没有身份、
# 没有 storage.json、没有保险库，退出码却是 0。`--quiet` 该压的是絮叨，不是
# "你装的东西有一半不能用"。
notice() {
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style --foreground 214 "⚠ $*" >&2
  else
    echo -e "\033[1;33m⚠\033[0m $*" >&2
  fi
}

run_with_spinner() {
  local title="$1"
  shift
  local exit_code=0
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ] && [ "$QUIET" -eq 0 ]; then
    local err_log="$TMP/gum-error.log"
    # Execute the command inside a bash subshell to securely pipe its output to a log file
    # while preserving the exact argument vector ($@) without stringification loss.
    if ! gum spin --spinner dot --title "$title" -- bash -c "\"\$@\" > \"\$0\" 2>&1" "$err_log" "$@"; then
      exit_code=1
    fi
    if [ "$exit_code" -ne 0 ]; then
      err "Command failed: $*"
      [ -f "$err_log" ] && cat "$err_log" >&2
      return $exit_code
    fi
  else
    info "$title"
    "$@" || return $?
  fi
}

# Draw a box around text with automatic width calculation.
# Uses Unicode double-line box characters for consistent visual structure.
# Responsive: clamps to terminal width and truncates long lines.
# Usage: draw_box "color_code" "line1" "line2" ...
draw_box() {
  local color="$1"
  shift
  local lines=("$@")
  local max_width=0
  local esc
  esc=$(printf '\033')
  local strip_ansi_sed="s/${esc}\\[[0-9;]*m//g"

  for line in "${lines[@]}"; do
    local stripped
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
    local len=${#stripped}
    if [ "$len" -gt "$max_width" ]; then
      max_width=$len
    fi
  done

  # Clamp box width to terminal width (leave room for box chars: 2 borders + 4 padding).
  local term_width
  term_width=$(tput cols 2>/dev/null || echo 80)
  local max_content_width=$((term_width - 6))
  if [ "$max_content_width" -lt 20 ]; then
    max_content_width=20
  fi
  if [ "$max_width" -gt "$max_content_width" ]; then
    max_width=$max_content_width
  fi

  local inner_width=$((max_width + 4))
  local border=""
  for ((i=0; i<inner_width; i++)); do
    border+="═"
  done

  printf "\033[%sm╔%s╗\033[0m\n" "$color" "$border"

  for line in "${lines[@]}"; do
    local stripped
    stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
    local len=${#stripped}
    # Truncate lines that exceed the available width.
    if [ "$len" -gt "$max_width" ]; then
      # Truncate the visible (stripped) content and re-apply to raw line.
      # For simplicity, cut raw line bytes; ANSI codes near the cut may break
      # but this is acceptable for a cosmetic display function.
      line=$(printf '%b' "$line" | cut -c1-"$max_width")
      stripped=$(printf '%b' "$line" | LC_ALL=C sed "$strip_ansi_sed")
      len=${#stripped}
    fi
    local padding=$((max_width - len))
    local pad_str=""
    for ((i=0; i<padding; i++)); do
      pad_str+=" "
    done
    printf "\033[%sm║\033[0m  %b%s  \033[%sm║\033[0m\n" "$color" "$line" "$pad_str" "$color"
  done

  printf "\033[%sm╚%s╝\033[0m\n" "$color" "$border"
}

# ═══════════════════════════════════════════════════════════════════════════════
# Proxy Support
# ═══════════════════════════════════════════════════════════════════════════════

PROXY_ARGS=()

setup_proxy() {
  PROXY_ARGS=()
  if [[ -n "${HTTPS_PROXY:-}" ]]; then
    PROXY_ARGS=(--proxy "$HTTPS_PROXY")
    info "Using HTTPS proxy: $HTTPS_PROXY"
  elif [[ -n "${HTTP_PROXY:-}" ]]; then
    PROXY_ARGS=(--proxy "$HTTP_PROXY")
    info "Using HTTP proxy: $HTTP_PROXY"
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Provider Detection
# ═══════════════════════════════════════════════════════════════════════════════

DETECTED_PROVIDERS=()
CLAUDE_VERSION=""
CODEX_VERSION=""
GEMINI_VERSION=""
CURSOR_VERSION=""
AIDER_VERSION=""
AMP_VERSION=""
OPENCODE_VERSION=""

print_provider_scan_notice() {
  [ "$QUIET" -eq 1 ] && return 0

  local line1="Scanning for installed coding agent providers..."
  local line2="ags converts sessions between detected providers."

  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    echo ""
    gum style \
      --border normal \
      --border-foreground 244 \
      --padding "0 1" \
      "$(gum style --foreground 212 --bold 'Provider scan')" \
      "$(gum style --foreground 247 "$line1")" \
      "$(gum style --foreground 245 "$line2")"
    echo ""
  else
    echo ""
    draw_box "0;36" "$line1" "$line2"
    echo ""
  fi
}

try_version() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || return 0

  local timeout_secs="${PROVIDER_VERSION_TIMEOUT:-1}"
  if ! [[ "$timeout_secs" =~ ^[0-9]+$ ]]; then
    timeout_secs=1
  fi

  if command -v timeout >/dev/null 2>&1; then
    timeout "$timeout_secs" "$cmd" --version 2>/dev/null | head -1 || true
  elif command -v gtimeout >/dev/null 2>&1; then
    gtimeout "$timeout_secs" "$cmd" --version 2>/dev/null | head -1 || true
  else
    "$cmd" --version 2>/dev/null | head -1 || true
  fi
}

detect_providers() {
  DETECTED_PROVIDERS=()

  # Claude Code (cc)
  if [[ -d "$CLAUDE_CONFIG_ROOT" ]] || command -v claude &>/dev/null; then
    DETECTED_PROVIDERS+=("claude-code")
    CLAUDE_VERSION=$(try_version claude)
  fi

  # Codex CLI (cod)
  if [[ -d "$CODEX_CONFIG_ROOT" ]] || command -v codex &>/dev/null ||
     command -v codext &>/dev/null; then
    DETECTED_PROVIDERS+=("codex")
    CODEX_VERSION=$(try_version codex)
  fi

  # Gemini CLI (gmi)
  if [[ -d "$HOME/.gemini" ]] || [[ -d "$HOME/.gemini-cli" ]] || command -v gemini &>/dev/null; then
    DETECTED_PROVIDERS+=("gemini")
    GEMINI_VERSION=$(try_version gemini)
  fi

  # Cursor (cur)
  local cursor_settings_mac="$HOME/Library/Application Support/Cursor/User/settings.json"
  local cursor_settings_linux="$HOME/.config/Cursor/User/settings.json"
  if [[ -d "$HOME/.cursor" ]] || [[ -f "$cursor_settings_mac" ]] || [[ -f "$cursor_settings_linux" ]] || command -v cursor &>/dev/null; then
    DETECTED_PROVIDERS+=("cursor")
    CURSOR_VERSION=$(try_version cursor)
  fi

  # Cline (cln)
  if [[ -d "$HOME/.config/Code/User/globalStorage/saoudrizwan.claude-dev" ]]; then
    DETECTED_PROVIDERS+=("cline")
  fi

  # Aider (aid)
  if command -v aider &>/dev/null; then
    DETECTED_PROVIDERS+=("aider")
    AIDER_VERSION=$(try_version aider)
  fi

  # Amp (amp)
  if [[ -d "$HOME/.local/share/amp" ]] || command -v amp &>/dev/null; then
    DETECTED_PROVIDERS+=("amp")
    AMP_VERSION=$(try_version amp)
  fi

  # OpenCode (opc)
  if [[ -d "$HOME/.opencode" ]] || command -v opencode &>/dev/null; then
    DETECTED_PROVIDERS+=("opencode")
    OPENCODE_VERSION=$(try_version opencode)
  fi

  # ChatGPT (gpt)
  if [[ -d "$HOME/.chatgpt" ]]; then
    DETECTED_PROVIDERS+=("chatgpt")
  fi
}

print_detected_providers() {
  if [[ ${#DETECTED_PROVIDERS[@]} -eq 0 ]]; then
    warn "No coding agent providers detected"
    info "Install at least two providers to use ags for session conversion"
    return
  fi

  local count=${#DETECTED_PROVIDERS[@]}
  local plural=""
  [[ $count -gt 1 ]] && plural="s"

  format_provider_line() {
    local name="$1"
    local alias="$2"
    local ver="$3"
    local ver_info=""
    [[ -n "$ver" ]] && ver_info=" ($ver)"
    if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
      gum style --foreground 42 "  ✓ ${name} [${alias}]${ver_info}"
    else
      echo -e "  \033[0;32m✓\033[0m ${name} [${alias}]${ver_info}"
    fi
  }

  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    echo ""
    gum style --foreground 39 --bold "Detected ${count} Provider${plural} (ags conversion targets):"
  else
    echo ""
    echo -e "\033[1;34mDetected ${count} Provider${plural} (ags conversion targets):\033[0m"
  fi

  for provider in "${DETECTED_PROVIDERS[@]}"; do
    case "$provider" in
      claude-code) format_provider_line "Claude Code" "cc" "$CLAUDE_VERSION" ;;
      codex)       format_provider_line "Codex CLI"   "cod" "$CODEX_VERSION" ;;
      gemini)      format_provider_line "Gemini CLI"  "gmi" "$GEMINI_VERSION" ;;
      cursor)      format_provider_line "Cursor"      "cur" "$CURSOR_VERSION" ;;
      cline)       format_provider_line "Cline"       "cln" "" ;;
      aider)       format_provider_line "Aider"       "aid" "$AIDER_VERSION" ;;
      amp)         format_provider_line "Amp"         "amp" "$AMP_VERSION" ;;
      opencode)    format_provider_line "OpenCode"    "opc" "$OPENCODE_VERSION" ;;
      chatgpt)     format_provider_line "ChatGPT"     "gpt" "" ;;
    esac
  done
  echo ""

  if [ "$count" -ge 2 ]; then
    info "ags convert can move a session between any pair of detected providers"
  else
    info "Install a second provider to enable cross-provider session conversion"
  fi
}

# Returns 0 if a provider slug is present in DETECTED_PROVIDERS.
has_provider() {
  local needle="$1"
  local provider=""
  for provider in "${DETECTED_PROVIDERS[@]}"; do
    if [ "$provider" = "$needle" ]; then
      return 0
    fi
  done
  return 1
}

# ═══════════════════════════════════════════════════════════════════════════════
# Agent Auto-Configuration (Skills + Wrapper Commands)
# ═══════════════════════════════════════════════════════════════════════════════

AGS_CONVERT_SKILL_ARCHIVE=""

write_inline_skill() {
  local dest="$1"
  mkdir -p "$dest"
  cat > "$dest/SKILL.md" <<'SKILL_EOF'
---
name: ags-convert
description: >-
  Cross Agent Session Resumer. Convert and resume sessions across Claude Code,
  Codex, Gemini, and other providers.
---

# ags convert — 跨 Agent 会话转换

Use `ags convert` when you need to keep working on the same session but switch providers.

## Fast Path

```bash
ags convert list
ags convert info <session-id>
ags convert -cc <session-id>   # open in Claude Code
ags convert -cod <session-id>  # open in Codex
ags convert -gmi <session-id>  # open in Gemini
```

## Helpful Commands

```bash
ags convert providers
ags convert list --workspace "$(pwd)" --sort date --limit 20
ags convert resume cod <session-id> --source cc
ags convert info <session-id> --json
```

## Notes

- `ags convert list` is project-scoped to your current working directory by default.
- `-cc`, `-cod`, and `-gmi` auto-detect source provider from the session ID.
- Use `--json` output mode for automation.
SKILL_EOF
}

download_skill_archive() {
  [ "$NO_SKILL" -eq 1 ] && return 1
  [ -z "$OFFLINE_TARBALL" ] || {
    SKILL_ARCHIVE_STATUS="bundled inline skill (offline)"
    return 1
  }

  local dest="$TMP/ags-convert-skill.tar.gz"
  local urls=(
    "https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/skill.tar.gz"
    "https://github.com/${OWNER}/${REPO}/releases/latest/download/skill.tar.gz"
  )
  local url=""
  for url in "${urls[@]}"; do
    if curl -fsSL "${PROXY_ARGS[@]}" "$url" -o "$dest" 2>/dev/null; then
      if tar -tzf "$dest" >/dev/null 2>&1; then
        AGS_CONVERT_SKILL_ARCHIVE="$dest"
        SKILL_ARCHIVE_STATUS="downloaded (${url})"
        return 0
      fi
    fi
  done
  SKILL_ARCHIVE_STATUS="bundled inline skill"
  return 1
}

install_skill_for_agent() {
  local agent_label="$1"
  local skills_root="$2"
  local status_var="$3"

  if [ "$NO_SKILL" -eq 1 ]; then
    printf -v "$status_var" '%s' "skipped (--no-skill)"
    return 0
  fi
  if checkpoint_path_has_symlink "$skills_root/ags-convert"; then
    printf -v "$status_var" '%s' "refused (symbolic-link path)"
    warn "$agent_label skill path contains a symbolic link: $skills_root/ags-convert"
    return 0
  fi

  if [ -n "$AGS_CONVERT_SKILL_ARCHIVE" ]; then
    mkdir -p "$skills_root"
    if tar -xzf "$AGS_CONVERT_SKILL_ARCHIVE" -C "$skills_root" 2>/dev/null \
      && [ -f "$skills_root/ags-convert/SKILL.md" ]; then
      printf -v "$status_var" '%s' "installed (release skill.tar.gz)"
      return 0
    fi
  fi

  local skill_dir="$skills_root/ags-convert"
  write_inline_skill "$skill_dir"
  if [ -f "$skill_dir/SKILL.md" ]; then
    printf -v "$status_var" '%s' "installed (inline fallback)"
  else
    printf -v "$status_var" '%s' "failed"
    warn "$agent_label skill install failed"
  fi
}

configure_agent_skills() {
  if [ "$NO_CONFIGURE" -eq 1 ]; then
    CLAUDE_SKILL_STATUS="skipped (--no-configure)"
    CODEX_SKILL_STATUS="skipped (--no-configure)"
    SKILL_ARCHIVE_STATUS="skipped (--no-configure)"
    return 0
  fi

  if [ "$NO_SKILL" -eq 1 ]; then
    SKILL_ARCHIVE_STATUS="skipped (--no-skill)"
  fi

  download_skill_archive || true

  if has_provider "claude-code" || [ -d "$CLAUDE_CONFIG_ROOT" ] || command -v claude >/dev/null 2>&1; then
    install_skill_for_agent "Claude Code" "$CLAUDE_CONFIG_ROOT/skills" CLAUDE_SKILL_STATUS
  else
    CLAUDE_SKILL_STATUS="not-detected"
  fi

  if has_provider "codex" || [ -d "$CODEX_CONFIG_ROOT" ] ||
   command -v codex >/dev/null 2>&1 || command -v codext >/dev/null 2>&1; then
    install_skill_for_agent "Codex" "$CODEX_CONFIG_ROOT/skills" CODEX_SKILL_STATUS
  else
    CODEX_SKILL_STATUS="not-detected"
  fi
}

status_path() {
  local path="$1"
  case "$path" in
    "$HOME"/*) printf '%s/%s' '~' "${path#"$HOME"/}" ;;
    *) printf '%s' "$path" ;;
  esac
}

install_wrapper_command() {
  local alias_name="$1"
  local target_name="$2"
  local status_var="$3"
  local wrapper_path="$DEST/$alias_name"
  local marker="# ags-installer-wrapper"
  # 改名之前写下的壳带的是旧标记。不认它的话，那些机器上的壳会被下面的分支当成
  # "手写的"而保留，从此再也收不到任何一次壳的修改。
  local legacy_marker="# casr-installer-wrapper"
  local target_path=""

  if [ "$NO_CONFIGURE" -eq 1 ]; then
    printf -v "$status_var" '%s' "skipped (--no-configure)"
    return 0
  fi

  if ! target_path=$(command -v "$target_name" 2>/dev/null); then
    printf -v "$status_var" '%s' "skipped (missing '$target_name')"
    return 0
  fi

  if command -v "$alias_name" >/dev/null 2>&1; then
    local current_alias_path=""
    current_alias_path=$(command -v "$alias_name" 2>/dev/null || true)
    if [ "$current_alias_path" != "$wrapper_path" ]; then
      printf -v "$status_var" '%s' "already exists on PATH ($(status_path "$current_alias_path"))"
      return 0
    fi
  fi

  if [ -f "$wrapper_path" ] &&
     ! grep -Fq "$marker" "$wrapper_path" 2>/dev/null &&
     ! grep -Fq "$legacy_marker" "$wrapper_path" 2>/dev/null; then
    printf -v "$status_var" '%s' "preserved unmanaged ($(status_path "$wrapper_path"))"
    return 0
  fi

  cat > "$wrapper_path" <<EOF
#!/usr/bin/env bash
$marker
exec "${target_path}" "\$@"
EOF
  chmod 0755 "$wrapper_path"
  printf -v "$status_var" '%s' "installed ($(status_path "$wrapper_path") -> $target_name)"
}

configure_provider_wrappers() {
  install_wrapper_command "cc" "claude" CC_WRAPPER_STATUS
  install_wrapper_command "cod" "codex" COD_WRAPPER_STATUS
  install_wrapper_command "gmi" "gemini" GMI_WRAPPER_STATUS
}

checkpoint_path_has_symlink() {
  local path="$1" current="/" relative component
  [[ "$path" == /* ]] || return 0
  relative="${path#/}"
  while [ -n "$relative" ]; do
    component="${relative%%/*}"
    if [ "$component" = "$relative" ]; then
      relative=""
    else
      relative="${relative#*/}"
    fi
    if [ "$current" = / ]; then
      current="/$component"
    else
      current="$current/$component"
    fi
    [ ! -L "$current" ] || return 0
  done
  return 1
}

retire_legacy_casr_binary() {
  # 二进制以前叫 `ags`，`ags` 只是它旁边一个四行的壳
  # （`exec "$script_dir/casr" checkpoint "$@"`）。现在二进制自己就叫 `ags`，
  # 所以那个壳的位置**正是新二进制要落的位置**——装完之后再写一次壳，等于把刚
  # 装好的二进制覆盖成一个指向已经不存在的 `ags` 的壳。这个函数因此只做减法。
  #
  # 旧的 `ags` 留在那儿不会坏事，但它是 6 MiB 的死重量，而且 PATH 里多一个叫
  # 这个名字的东西会让人以为它还有用。
  local legacy="$DEST/casr"
  local stale_skill

  if [ "$NO_CONFIGURE" -eq 1 ]; then
    AGS_WRAPPER_STATUS="skipped (--no-configure)"
    return 0
  fi

  # 只动我们自己装的那个：普通文件、就在我们的目标目录里、不是别人的符号链接。
  if [ ! -e "$legacy" ]; then
    AGS_WRAPPER_STATUS="ags is the binary now"
    return 0
  fi
  if [ -L "$legacy" ] || [ ! -f "$legacy" ]; then
    AGS_WRAPPER_STATUS="left $(status_path "$legacy") alone (not a plain file)"
    return 0
  fi
  if rm -f "$legacy"; then
    AGS_WRAPPER_STATUS="removed the old $(status_path "$legacy")"
  else
    AGS_WRAPPER_STATUS="could not remove $(status_path "$legacy")"
  fi
}

# 转换那份 skill 以前叫 `ags`，现在叫 `ags-convert`。装了新的就把旧的收走——两份
# 同时摆在 skills 目录里，Agent 会把它们当成两个不同的能力，而它们说的是同一件事。
retire_legacy_casr_skill() {
  local root stale
  for root in "$CODEX_CONFIG_ROOT/skills" "$CLAUDE_CONFIG_ROOT/skills"; do
    stale="$root/ags"
    # 只删我们自己写的那份：里面得有我们生成的 SKILL.md，而且不能是符号链接。
    [ -d "$stale" ] || continue
    [ ! -L "$stale" ] || continue
    [ -f "$stale/SKILL.md" ] || continue
    grep -Fq 'ags convert' "$stale/SKILL.md" 2>/dev/null ||
      grep -Fq 'ags' "$stale/SKILL.md" 2>/dev/null || continue
    rm -rf -- "$stale"
  done
}

install_checkpoint_skill() {
  local root="$1" status_var="$2" skill_dir
  local staged="$TMP/ags-skill-${status_var}"
  skill_dir="$root/ags"

  if [ "$NO_SKILL" -eq 1 ]; then
    printf -v "$status_var" '%s' "skipped (--no-skill)"
    return 0
  fi
  if checkpoint_path_has_symlink "$skill_dir"; then
    printf -v "$status_var" '%s' "refused (symbolic-link path)"
    warn "Checkpoint skill path contains a symbolic link: $skill_dir"
    return 0
  fi

  mkdir -p "$staged/agents"
  "$DEST/$BINARY_NAME" checkpoint-asset skill > "$staged/SKILL.md"
  "$DEST/$BINARY_NAME" checkpoint-asset openai-agent > "$staged/agents/openai.yaml"
  mkdir -p "$skill_dir/agents"
  install -m 0644 "$staged/SKILL.md" "$skill_dir/SKILL.md"
  install -m 0644 "$staged/agents/openai.yaml" "$skill_dir/agents/openai.yaml"
  printf -v "$status_var" '%s' "installed ($(status_path "$skill_dir"))"
}

write_checkpoint_hooks() {
  local file="$1" kind="$2" directory temporary source
  local command="$DEST/$BINARY_NAME hook"
  local legacy_ags="$DEST/ags hook"
  local legacy_session="$DEST/agent-session hook"

  if checkpoint_path_has_symlink "$file"; then
    warn "Checkpoint hook path contains a symbolic link: $file"
    return 1
  fi
  directory="$(dirname "$file")"
  mkdir -p "$directory"
  source="$TMP/hooks-${kind}.json"
  if [ -s "$file" ]; then
    jq -e 'type == "object"' "$file" >/dev/null ||
      { warn "Checkpoint hooks skipped; invalid JSON object: $file"; return 1; }
    cp "$file" "$source"
  else
    printf '{}\n' > "$source"
  fi

  temporary="$(mktemp "$directory/.ags-hooks.XXXXXX")"
  jq --arg command "$command" --arg legacy_ags "$legacy_ags" \
    --arg legacy_session "$legacy_session" --arg kind "$kind" '
      .hooks = (.hooks // {}) |
      reduce ["Stop", "SessionStart"][] as $event (.;
        .hooks[$event] = (
          [(.hooks[$event] // [])[] |
            .hooks = [
              (.hooks // [])[] |
              select(
                (.command // "") != $command and
                (.command // "") != $legacy_ags and
                (.command // "") != $legacy_session and
                (.command // "") != "/usr/local/bin/ags hook" and
                (.command // "") != "/usr/local/bin/agent-session hook"
              )
            ] |
            select((.hooks | length) > 0)
          ] + [{
            hooks: [{type: "command", command: $command, timeout: 300}]
          }]
        )
      ) |
      if $kind == "claude" then
        .skillOverrides = (.skillOverrides // {}) |
        del(.skillOverrides["agent-session"]) |
        .skillOverrides.ags = "user-invocable-only"
      else . end
    ' "$source" > "$temporary"
  chmod 0600 "$temporary"
  mv -f "$temporary" "$file"
}

checkpoint_dependencies_ready() {
  local missing=() command
  for command in age age-keygen column flock git jq rclone rsync ssh ssh-keygen tar zstd; do
    command -v "$command" >/dev/null 2>&1 || missing+=("$command")
  done
  if [ "${BASH_VERSINFO[0]}" -lt 4 ]; then
    missing+=("bash>=4")
  fi
  if [ "${#missing[@]}" -gt 0 ]; then
    notice "Checkpoint runtime not initialized; missing: ${missing[*]}"
    return 1
  fi
  return 0
}

run_checkpoint_runtime() {
  "$DEST/$BINARY_NAME" "$@"
}


# Bring codext to the current release, installing it if this host has none.
#
# codext is Codex with the credential pool wired in, and AGS launches it in
# preference to stock Codex whenever it is on PATH — so an install that skips it
# leaves every session quietly running on the machine's own account.
#
# It rides its own release train, which is why this runs after the core install
# transaction has been committed and can only report, never fail the install.
# `ags codext-update` is the same function `ags update` calls; reaching for
# `ags update` here instead would also self-update ags and undo an explicit
# `--version` seconds after it landed.
configure_codext_only() {
  if [ "$NO_CONFIGURE" -eq 1 ]; then
    CODEXT_STATUS="skipped (--no-configure)"
    return 0
  fi
  if [ -n "$OFFLINE_TARBALL" ]; then
    CODEXT_STATUS="skipped (--offline)"
    return 0
  fi
  run_checkpoint_runtime codext-update || true
  # Decide who owns the name `codex`, now that codext is on disk.
  #
  # Here and nowhere else: this is the one step allowed to ask, and to move an
  # existing codex aside if the answer is yes. `ags init` deliberately only
  # fills an empty name — displacing someone's binary is not something a
  # re-runnable command should ever do.
  run_checkpoint_runtime codex-name || true
  # The stamp, not `codext --version`: that reports the upstream Codex version
  # the fork is built on, so two codext releases can read as the same thing.
  if [ -s "$CODEXT_RELEASE_STAMP" ]; then
    CODEXT_STATUS="$(head -n 1 "$CODEXT_RELEASE_STAMP")"
  elif command -v codext >/dev/null 2>&1; then
    CODEXT_STATUS="installed (release unknown)"
  else
    CODEXT_STATUS="not installed"
  fi
}

configure_checkpoints() {
  local init_output
  if [ "$NO_CONFIGURE" -eq 1 ]; then
    AGS_WRAPPER_STATUS="skipped (--no-configure)"
    AGS_CODEX_SKILL_STATUS="skipped (--no-configure)"
    AGS_CLAUDE_SKILL_STATUS="skipped (--no-configure)"
    AGS_HOOK_STATUS="skipped (--no-configure)"
    AGS_INIT_STATUS="skipped (--no-configure)"
    return 0
  fi

  retire_legacy_casr_binary
  retire_legacy_casr_skill
  install_checkpoint_skill "$CODEX_CONFIG_ROOT/skills" AGS_CODEX_SKILL_STATUS
  install_checkpoint_skill "$CLAUDE_CONFIG_ROOT/skills" AGS_CLAUDE_SKILL_STATUS
  if write_checkpoint_hooks "$CODEX_CONFIG_ROOT/hooks.json" codex &&
     write_checkpoint_hooks "$CLAUDE_CONFIG_ROOT/settings.json" claude; then
    AGS_HOOK_STATUS="installed"
  else
    AGS_HOOK_STATUS="partial; see warnings"
  fi

  if ! checkpoint_dependencies_ready; then
    AGS_INIT_STATUS="skipped (missing dependencies)"
    return 0
  fi

  if [ -n "$CHECKPOINT_IDENTITY" ]; then
    init_output="$(run_checkpoint_runtime init --identity "$CHECKPOINT_IDENTITY")"
  else
    init_output="$(run_checkpoint_runtime init)"
  fi
  if grep -Fqx 'status=initialized' <<< "$init_output"; then
    AGS_INIT_STATUS="initialized"
  else
    AGS_INIT_STATUS="failed"
    err "Checkpoint initialization returned an unexpected result"
    return 1
  fi
}

configure_agents() {
  configure_provider_wrappers
  configure_agent_skills
  configure_checkpoints
}

# ═══════════════════════════════════════════════════════════════════════════════
# Version Resolution
# ═══════════════════════════════════════════════════════════════════════════════

resolve_version() {
  if [ -n "$VERSION" ]; then return 0; fi
  if [ -n "$OFFLINE_TARBALL" ]; then
    info "Offline mode; the version will be read from the verified local artifact"
    return 0
  fi

  info "Resolving latest version..."
  local latest_url="https://api.github.com/repos/${OWNER}/${REPO}/releases/latest"
  local tag
  if tag=$(curl -fsSL --connect-timeout 5 "${PROXY_ARGS[@]}" \
    -H "Accept: application/vnd.github.v3+json" "$latest_url" 2>/dev/null \
    | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'); then
    if [ -n "$tag" ]; then
      VERSION="$tag"
      info "Resolved latest version: $VERSION"
      return 0
    fi
  fi

  # Fallback: redirect-based resolution (handles GitHub API rate limits)
  local redirect_url="https://github.com/${OWNER}/${REPO}/releases/latest"
  if tag=$(curl -fsSL "${PROXY_ARGS[@]}" -o /dev/null -w '%{url_effective}' "$redirect_url" 2>/dev/null \
    | sed -E 's|.*/tag/||'); then
    if [ -n "$tag" ] && [[ "$tag" =~ ^v[0-9] ]] && [[ "$tag" != *"/"* ]]; then
      VERSION="$tag"
      info "Resolved latest version via redirect: $VERSION"
      return 0
    fi
  fi

  # A source build takes the default branch, not a tag, so it needs no version.
  # `check_installed_version` rejects an empty target, so the already-installed
  # short-circuit stays off and the build still runs.
  if [ "$FROM_SOURCE" -eq 1 ]; then
    info "Could not resolve a release version; building from source instead"
    return 0
  fi

  # Never guess. A guessed version is worse than no install: the guess is
  # compared against what is already on disk, an older guess always loses that
  # comparison, and the installer then reports "already installed" and exits 0
  # having updated nothing. A user asking to be updated must not be told they
  # are up to date because a lookup failed.
  err "Could not resolve the latest ${OWNER}/${REPO} release."
  err "Neither the GitHub API nor the /releases/latest redirect returned a tag."
  err "This is expected if the repository has no releases yet, is private and"
  err "this host is unauthenticated, or the network blocks github.com."
  err "Pass --version vX.Y.Z, --from-source, or --offline TARBALL."
  exit 1
}

# ═══════════════════════════════════════════════════════════════════════════════
# Platform Detection
# ═══════════════════════════════════════════════════════════════════════════════

OS=""
ARCH=""
TARGET=""

detect_platform() {
  OS=$(uname -s | tr '[:upper:]' '[:lower:]')
  ARCH=$(uname -m)
  case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    arm64|aarch64) ARCH="aarch64" ;;
    *) warn "Unknown architecture $ARCH, using as-is" ;;
  esac

  # WSL detection
  if [[ "$OS" == "linux" ]] && grep -qi microsoft /proc/version 2>/dev/null; then
    warn "WSL detected. ags will work normally; provider paths may differ from Windows host"
  fi

  TARGET=""
  case "${OS}-${ARCH}" in
    linux-x86_64)   TARGET="x86_64-unknown-linux-musl" ;;
    linux-aarch64)  TARGET="aarch64-unknown-linux-musl" ;;
    darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
    darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
    *) :;;
  esac

  if [ -z "$TARGET" ] && [ "$FROM_SOURCE" -eq 0 ] && [ -z "$ARTIFACT_URL" ] && [ -z "$OFFLINE_TARBALL" ]; then
    warn "No prebuilt binary for ${OS}/${ARCH}; falling back to build-from-source"
    FROM_SOURCE=1
  fi
}

set_artifact_url() {
  TAR=""
  URL=""
  if [ "$FROM_SOURCE" -eq 0 ] && [ -z "$OFFLINE_TARBALL" ]; then
    if [ -n "$ARTIFACT_URL" ]; then
      TAR=$(basename "$ARTIFACT_URL")
      URL="$ARTIFACT_URL"
    elif [ -n "$TARGET" ]; then
      TAR="ags-${TARGET}.tar.xz"
      URL="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${TAR}"
    else
      warn "No prebuilt artifact for ${OS}/${ARCH}; falling back to build-from-source"
      FROM_SOURCE=1
    fi
  fi
}

# ═══════════════════════════════════════════════════════════════════════════════
# Preflight Checks
# ═══════════════════════════════════════════════════════════════════════════════

check_disk_space() {
  local min_kb=10240  # 10 MB
  local path="$DEST"
  if [ ! -d "$path" ]; then
    path=$(dirname "$path")
  fi
  if command -v df >/dev/null 2>&1; then
    local avail_kb
    avail_kb=$(df -Pk "$path" | awk 'NR==2 {print $4}')
    if [ -n "$avail_kb" ] && [ "$avail_kb" -lt "$min_kb" ]; then
      err "Insufficient disk space in $path (need at least 10MB)"
      exit 1
    fi
  else
    warn "df not found; skipping disk space check"
  fi
}

check_write_permissions() {
  if [ ! -d "$DEST" ]; then
    if ! mkdir -p "$DEST" 2>/dev/null; then
      err "Cannot create $DEST (insufficient permissions)"
      err "Try running with sudo or choose a writable --dest"
      exit 1
    fi
  fi
  if [ ! -w "$DEST" ]; then
    err "No write permission to $DEST"
    err "Try running with sudo or choose a writable --dest"
    exit 1
  fi
}

check_existing_install() {
  if [ -x "$DEST/$BINARY_NAME" ]; then
    local current
    current=$("$DEST/$BINARY_NAME" --version 2>/dev/null | head -1 || echo "")
    if [ -n "$current" ]; then
      info "Existing ags detected: $current"
    fi
  fi
}

check_network() {
  if [ -n "$OFFLINE_TARBALL" ]; then
    info "Offline mode; skipping network preflight"
    return 0
  fi
  if [ "$FROM_SOURCE" -eq 1 ]; then
    return 0
  fi
  if [ -z "$URL" ]; then
    return 0
  fi
  if ! command -v curl >/dev/null 2>&1; then
    warn "curl not found; skipping network check"
    return 0
  fi
  if ! curl -fsSL "${PROXY_ARGS[@]}" --connect-timeout 3 --max-time 5 -o /dev/null "$URL" 2>/dev/null; then
    warn "Network check failed for $URL"
    warn "Continuing; download may fail"
  fi
}

preflight_installer_tools() {
  # 这三样是安装事务自己的依赖，不是谁的附带要求：
  #   jq            读写和校验 install-transaction 日志
  #   node          durable_sync_path 靠它把改动刷到盘上
  #   sha256sum/shasum  比对候选二进制和回滚副本
  # 少一样，失败会发生在写了一半的地方，而不是发生在动手之前。
  command -v jq >/dev/null 2>&1 || {
    err "The installer requires jq"
    exit 1
  }
  command -v node >/dev/null 2>&1 || {
    err "The installer requires Node.js"
    exit 1
  }
  if ! command -v sha256sum >/dev/null 2>&1 &&
     ! command -v shasum >/dev/null 2>&1; then
    err "The installer requires sha256sum or shasum"
    exit 1
  fi
}

installer_sha256_file() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | cut -d' ' -f1
  else
    shasum -a 256 "$file" | cut -d' ' -f1
  fi
}

durable_sync_path() {
  local path="$1"
  node - "$path" <<'NODE'
const fs = require("node:fs");
const path = require("node:path");

const target = process.argv[2];
const targetStat = fs.lstatSync(target);
if (targetStat.isSymbolicLink()) process.exit(1);

function flush(entry, directory) {
  const descriptor = fs.openSync(entry, "r");
  try {
    fs.fsyncSync(descriptor);
  } catch (error) {
    if (
      !directory ||
      !["EINVAL", "ENOTSUP", "EISDIR"].includes(error?.code)
    ) {
      throw error;
    }
  } finally {
    fs.closeSync(descriptor);
  }
}

flush(target, targetStat.isDirectory());
if (!targetStat.isDirectory()) {
  flush(path.dirname(target), true);
}
NODE
}

install_transaction_child_path() {
  local path="$1" kind="$2" parent name
  parent="$(dirname -- "$path")" || return 1
  name="$(basename -- "$path")" || return 1
  [[ "$parent" == "$DEST" &&
     "$name" == ".${BINARY_NAME}.${kind}."* &&
     "$name" != *$'\t'* &&
     "$name" != *$'\n'* &&
     "$name" != *$'\r'* ]]
}

validate_install_transaction() {
  [[ -f "$INSTALL_TRANSACTION_FILE" &&
     ! -L "$INSTALL_TRANSACTION_FILE" ]] ||
    return 1
  jq -e --arg binary "$DEST/$BINARY_NAME" '
      (keys | sort) ==
        ["binary_path", "candidate", "managed_by", "previous", "schema"] and
      .schema == 2 and
      .managed_by == "ags-installer" and
      .binary_path == $binary and
      (.candidate | keys | sort) == ["sha256", "stage_path"] and
      (.candidate.sha256 | test("^[0-9a-f]{64}$")) and
      (.candidate.stage_path | type) == "string" and
      (.previous | keys | sort) ==
        ["backup_path", "existed", "sha256"] and
      (.previous.existed | type) == "boolean" and
      (
        if .previous.existed
        then
          (.previous.sha256 | test("^[0-9a-f]{64}$")) and
          (.previous.backup_path | type) == "string"
        else
          .previous.sha256 == null and
          .previous.backup_path == null
        end
      )
    ' "$INSTALL_TRANSACTION_FILE" >/dev/null 2>&1 ||
    return 1

  local stage backup previous_existed
  stage="$(jq -er '.candidate.stage_path' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  install_transaction_child_path "$stage" install || return 1
  previous_existed="$(jq -er '
    .previous.existed |
    if type == "boolean" then tostring else error("invalid boolean") end
  ' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  if [[ "$previous_existed" == true ]]; then
    backup="$(jq -er '.previous.backup_path' \
      "$INSTALL_TRANSACTION_FILE")" || return 1
    install_transaction_child_path "$backup" rollback || return 1
  fi
}

write_install_transaction() {
  local stage="$1" candidate_sha="$2"
  local previous_sha= backup_json=null temporary
  [[ ! -e "$INSTALL_TRANSACTION_FILE" &&
     ! -L "$INSTALL_TRANSACTION_FILE" ]] ||
    return 1
  if [[ "$BINARY_PREEXISTED" -eq 1 ]]; then
    previous_sha="$(installer_sha256_file "$DEST/$BINARY_NAME")" ||
      return 1
    backup_json="$(jq -Rn --arg path "$BINARY_BACKUP" '$path')" ||
      return 1
  fi
  durable_sync_path "$stage" || return 1
  if [[ "$BINARY_PREEXISTED" -eq 1 ]]; then
    durable_sync_path "$BINARY_BACKUP" || return 1
  fi
  temporary="$(
    mktemp "$DEST/.${BINARY_NAME}.install-transaction.tmp.XXXXXX"
  )" || return 1
  if ! jq -n \
      --arg binary "$DEST/$BINARY_NAME" \
      --arg candidate_sha "$candidate_sha" \
      --arg stage "$stage" \
      --argjson previous_existed \
        "$([[ "$BINARY_PREEXISTED" -eq 1 ]] && printf true || printf false)" \
      --arg previous_sha "$previous_sha" \
      --argjson backup "$backup_json" '
        {
          schema:2,
          managed_by:"ags-installer",
          binary_path:$binary,
          candidate:{
            sha256:$candidate_sha,
            stage_path:$stage
          },
          previous:{
            existed:$previous_existed,
            sha256:(
              if $previous_existed then $previous_sha else null end
            ),
            backup_path:$backup
          }
        }
      ' > "$temporary"; then
    rm -f -- "$temporary"
    return 1
  fi
  chmod 600 "$temporary" || {
    rm -f -- "$temporary"
    return 1
  }
  mv -- "$temporary" "$INSTALL_TRANSACTION_FILE" || {
    rm -f -- "$temporary"
    return 1
  }
  INSTALL_TRANSACTION_ACTIVE=1
  durable_sync_path "$INSTALL_TRANSACTION_FILE"
}

clear_install_transaction_artifacts() {
  local stage backup previous_existed
  validate_install_transaction || return 1
  stage="$(jq -er '.candidate.stage_path' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  previous_existed="$(jq -er '
    .previous.existed |
    if type == "boolean" then tostring else error("invalid boolean") end
  ' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  backup=
  if [[ "$previous_existed" == true ]]; then
    backup="$(jq -er '.previous.backup_path' \
      "$INSTALL_TRANSACTION_FILE")" || return 1
  fi
  rm -f -- "$INSTALL_TRANSACTION_FILE" || return 1
  INSTALL_TRANSACTION_ACTIVE=0
  durable_sync_path "$DEST" || return 1
  [[ -z "$backup" ]] || rm -f -- "$backup"
  [[ ! -e "$stage" && ! -L "$stage" ]] || rm -f -- "$stage"
  BINARY_BACKUP=""
  BINARY_STAGE=""
}

commit_install_transaction() {
  local candidate_sha current_sha
  validate_install_transaction || return 1
  candidate_sha="$(jq -er '.candidate.sha256' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  [[ -f "$DEST/$BINARY_NAME" &&
     ! -L "$DEST/$BINARY_NAME" ]] ||
    return 1
  current_sha="$(installer_sha256_file "$DEST/$BINARY_NAME")" ||
    return 1
  [[ "$current_sha" == "$candidate_sha" ]] || return 1
  clear_install_transaction_artifacts
}

restore_install_transaction_if_safe() {
  local candidate_sha previous_existed previous_sha backup current_sha=
  local restore_stage
  validate_install_transaction || return 1
  candidate_sha="$(jq -er '.candidate.sha256' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  previous_existed="$(jq -er '
    .previous.existed |
    if type == "boolean" then tostring else error("invalid boolean") end
  ' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  if [[ -e "$DEST/$BINARY_NAME" || -L "$DEST/$BINARY_NAME" ]]; then
    [[ -f "$DEST/$BINARY_NAME" &&
       ! -L "$DEST/$BINARY_NAME" ]] ||
      return 1
    current_sha="$(installer_sha256_file "$DEST/$BINARY_NAME")" ||
      return 1
    [[ "$current_sha" == "$candidate_sha" ]] || {
      if [[ "$previous_existed" == true ]]; then
        previous_sha="$(jq -er '.previous.sha256' \
          "$INSTALL_TRANSACTION_FILE")" || return 1
        [[ "$current_sha" == "$previous_sha" ]] || return 1
      else
        return 1
      fi
    }
  fi

  if [[ "$previous_existed" == true ]]; then
    previous_sha="$(jq -er '.previous.sha256' \
      "$INSTALL_TRANSACTION_FILE")" || return 1
    backup="$(jq -er '.previous.backup_path' \
      "$INSTALL_TRANSACTION_FILE")" || return 1
    [[ -f "$backup" && ! -L "$backup" ]] || return 1
    [[ "$(installer_sha256_file "$backup")" == "$previous_sha" ]] ||
      return 1
    if [[ "$current_sha" != "$previous_sha" ]]; then
      restore_stage="$(
        mktemp "$DEST/.${BINARY_NAME}.restore.XXXXXX"
      )" || return 1
      if ! install -m 0755 "$backup" "$restore_stage" ||
         ! durable_sync_path "$restore_stage" ||
         ! mv -f -- "$restore_stage" "$DEST/$BINARY_NAME" ||
         ! durable_sync_path "$DEST/$BINARY_NAME"; then
        rm -f -- "$restore_stage"
        return 1
      fi
    fi
  elif [[ -e "$DEST/$BINARY_NAME" ||
          -L "$DEST/$BINARY_NAME" ]]; then
    rm -f -- "$DEST/$BINARY_NAME" || return 1
    durable_sync_path "$DEST" || return 1
  fi
  clear_install_transaction_artifacts
}

recover_pending_install_transaction() {
  local candidate_sha previous_existed previous_sha current_sha=
  if [[ ! -e "$INSTALL_TRANSACTION_FILE" &&
        ! -L "$INSTALL_TRANSACTION_FILE" ]]; then
    return 0
  fi
  validate_install_transaction || {
    err "Invalid pending installer transaction: $INSTALL_TRANSACTION_FILE"
    return 1
  }
  candidate_sha="$(jq -er '.candidate.sha256' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  previous_existed="$(jq -er '
    .previous.existed |
    if type == "boolean" then tostring else error("invalid boolean") end
  ' \
    "$INSTALL_TRANSACTION_FILE")" || return 1
  if [[ -e "$DEST/$BINARY_NAME" || -L "$DEST/$BINARY_NAME" ]]; then
    [[ -f "$DEST/$BINARY_NAME" &&
       ! -L "$DEST/$BINARY_NAME" ]] || {
      err "Pending install target is no longer a regular file"
      return 1
    }
    current_sha="$(installer_sha256_file "$DEST/$BINARY_NAME")" ||
      return 1
  fi

  if [[ -n "$current_sha" && "$current_sha" == "$candidate_sha" ]]; then
    # Keep the binary journal active and rejoin the ordinary post-install path,
    # which commits the ags binary.
    INSTALL_TRANSACTION_ACTIVE=1
    info "Resuming the interrupted ags installation"
    return 0
  fi

  if [[ "$previous_existed" == true ]]; then
    previous_sha="$(jq -er '.previous.sha256' \
      "$INSTALL_TRANSACTION_FILE")" || return 1
    if [[ -z "$current_sha" ]]; then
      if restore_install_transaction_if_safe; then
        warn "Restored the previous binary after interrupted activation"
        return 0
      fi
      err "Pending install target disappeared and could not be restored safely"
      return 1
    fi
    [[ -n "$current_sha" && "$current_sha" == "$previous_sha" ]] || {
      err "Pending install target does not match the candidate or previous binary"
      return 1
    }
  elif [[ -n "$current_sha" ]]; then
    err "Pending first-install target does not match the candidate binary"
    return 1
  fi
  clear_install_transaction_artifacts || return 1
  warn "Discarded an interrupted install before binary activation"
}

install_ags_binary() {
  local source="$1"
  local candidate_sha
  [ -x "$source" ] || {
    err "Binary is not executable: $source"
    return 1
  }
  [[ ! -e "$INSTALL_TRANSACTION_FILE" &&
     ! -L "$INSTALL_TRANSACTION_FILE" ]] || {
    err "A pending installer transaction must be recovered first"
    return 1
  }
  BINARY_STAGE="$(
    mktemp "$DEST/.${BINARY_NAME}.install.XXXXXX"
  )" || return 1
  install -m 0755 "$source" "$BINARY_STAGE" || return 1
  if [[ -e "$DEST/$BINARY_NAME" || -L "$DEST/$BINARY_NAME" ]]; then
    [ -f "$DEST/$BINARY_NAME" ] && [ ! -L "$DEST/$BINARY_NAME" ] || {
      err "Refusing to overwrite a non-regular binary target: $DEST/$BINARY_NAME"
      return 1
    }
    BINARY_BACKUP="$(
      mktemp "$DEST/.${BINARY_NAME}.rollback.XXXXXX"
    )" || return 1
    if ! cp -p "$DEST/$BINARY_NAME" "$BINARY_BACKUP"; then
      rm -f -- "$BINARY_BACKUP"
      BINARY_BACKUP=""
      return 1
    fi
    BINARY_PREEXISTED=1
  else
    BINARY_PREEXISTED=0
  fi
  candidate_sha="$(installer_sha256_file "$BINARY_STAGE")" || return 1
  if ! write_install_transaction "$BINARY_STAGE" "$candidate_sha"; then
    if [[ ! -e "$INSTALL_TRANSACTION_FILE" &&
          ! -L "$INSTALL_TRANSACTION_FILE" ]]; then
      [[ -z "$BINARY_BACKUP" ]] || rm -f -- "$BINARY_BACKUP"
    fi
    err "Cannot persist the installer transaction"
    return 1
  fi
  mv -f -- "$BINARY_STAGE" "$DEST/$BINARY_NAME" || return 1
  BINARY_STAGE=""
  durable_sync_path "$DEST/$BINARY_NAME"
}

preflight_checks() {
  info "Running preflight checks"
  check_disk_space
  check_write_permissions
  check_existing_install
  check_network
}

# ═══════════════════════════════════════════════════════════════════════════════
# Version Comparison
# ═══════════════════════════════════════════════════════════════════════════════

check_installed_version() {
  local target_version="$1"
  if [ ! -x "$DEST/$BINARY_NAME" ]; then
    return 1
  fi

  local installed_version
  installed_version=$("$DEST/$BINARY_NAME" --version 2>/dev/null | head -1 | sed -E 's/[^0-9]*([0-9]+\.[0-9]+\.[0-9]+).*/\1/')

  if [ -z "$installed_version" ]; then
    return 1
  fi

  local target_clean="${target_version#v}"
  local installed_clean="${installed_version#v}"

  INSTALLED_AGS_VERSION="$installed_clean"
  version_at_least "$installed_clean" "$target_clean"
}

version_at_least() {
  local installed="$1"
  local target="$2"
  local installed_major installed_minor installed_patch
  local target_major target_minor target_patch

  IFS=. read -r installed_major installed_minor installed_patch _ <<< "$installed"
  IFS=. read -r target_major target_minor target_patch _ <<< "$target"

  for part in \
    "$installed_major" "$installed_minor" "$installed_patch" \
    "$target_major" "$target_minor" "$target_patch"
  do
    [[ "$part" =~ ^[0-9]+$ ]] || return 1
  done

  if ((10#$installed_major != 10#$target_major)); then
    ((10#$installed_major > 10#$target_major))
    return $?
  fi
  if ((10#$installed_minor != 10#$target_minor)); then
    ((10#$installed_minor > 10#$target_minor))
    return $?
  fi
  ((10#$installed_patch >= 10#$target_patch))
}

# ═══════════════════════════════════════════════════════════════════════════════
# Checksum & Signature Verification
# ═══════════════════════════════════════════════════════════════════════════════

verify_checksum() {
  local file="$1"
  local expected="$2"
  local actual=""

  if [ ! -f "$file" ]; then
    err "File not found: $file"
    return 1
  fi

  if command -v sha256sum &>/dev/null; then
    actual=$(sha256sum "$file" | cut -d' ' -f1)
  elif command -v shasum &>/dev/null; then
    actual=$(shasum -a 256 "$file" | cut -d' ' -f1)
  else
    warn "No SHA256 tool found (sha256sum or shasum); skipping verification"
    return 0
  fi

  if [ "$actual" != "$expected" ]; then
    err "Checksum verification FAILED!"
    err "Expected: $expected"
    err "Got:      $actual"
    err "The downloaded file may be corrupted or tampered with."
    rm -f "$file"
    return 1
  fi

  ok "Checksum verified: ${actual:0:16}..."
  return 0
}

verify_sigstore_bundle() {
  local file="$1"
  local artifact_url="$2"

  if ! command -v cosign &>/dev/null; then
    warn "cosign not found; skipping signature verification (install cosign for stronger authenticity checks)"
    return 0
  fi

  local bundle_url="$SIGSTORE_BUNDLE_URL"
  if [ -z "$bundle_url" ]; then
    bundle_url="${artifact_url}.sigstore.json"
  fi

  local bundle_file=""
  bundle_file="$TMP/$(basename "$bundle_url")"
  info "Fetching sigstore bundle from ${bundle_url}"
  if ! curl -fsSL "${PROXY_ARGS[@]}" "$bundle_url" -o "$bundle_file" 2>/dev/null; then
    warn "Sigstore bundle not found; skipping signature verification"
    return 0
  fi

  if ! cosign verify-blob \
    --bundle "$bundle_file" \
    --certificate-identity-regexp "$COSIGN_IDENTITY_RE" \
    --certificate-oidc-issuer "$COSIGN_OIDC_ISSUER" \
    "$file" 2>/dev/null; then
    return 1
  fi

  ok "Signature verified (cosign)"
  return 0
}

# ═══════════════════════════════════════════════════════════════════════════════
# Rust Toolchain (for build-from-source)
# ═══════════════════════════════════════════════════════════════════════════════

ensure_rust() {
  if [ "${RUSTUP_INIT_SKIP:-0}" != "0" ]; then
    info "Skipping rustup install (RUSTUP_INIT_SKIP set)"
    return 0
  fi
  if command -v cargo >/dev/null 2>&1 && rustc --version 2>/dev/null | grep -q nightly; then return 0; fi
  if [ "$ASSUME_YES" -eq 1 ] || [ "$EASY" -eq 1 ]; then
    info "Auto-accepting Rust nightly install (--yes/--easy-mode)"
  else
    if [ -t 0 ]; then
      echo -n "Install Rust nightly via rustup? (y/N): "
      read -r ans
      case "$ans" in y|Y) :;; *) warn "Skipping rustup install"; return 0;; esac
    fi
  fi
  info "Installing rustup (nightly) — ags requires Rust nightly (edition 2024)"
  curl --proto '=https' --tlsv1.2 -sSf "${PROXY_ARGS[@]}" https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain nightly --profile minimal
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  export PATH="$HOME/.cargo/bin:$PATH"
}

# ═══════════════════════════════════════════════════════════════════════════════
# PATH Management
# ═══════════════════════════════════════════════════════════════════════════════

maybe_add_path() {
  case ":$PATH:" in
    *:"$DEST":*) return 0 ;;
    *)
      if [ "$EASY" -eq 1 ]; then
        UPDATED=0
        for rc in "$HOME/.zshrc" "$HOME/.bashrc"; do
          if [ -e "$rc" ] && [ -w "$rc" ]; then
            if ! grep -F "$DEST" "$rc" >/dev/null 2>&1; then
              echo "export PATH=\"$DEST:\$PATH\"" >> "$rc"
            fi
            UPDATED=1
          fi
        done
        if [ "$UPDATED" -eq 1 ]; then
          warn "PATH updated in ~/.zshrc/.bashrc; restart shell to use ags"
        else
          warn "Add $DEST to PATH to use ags"
        fi
      else
        warn "Add $DEST to PATH to use ags"
      fi
    ;;
  esac
}

# ═══════════════════════════════════════════════════════════════════════════════
# Shell Completions
# ═══════════════════════════════════════════════════════════════════════════════

detect_default_shell() {
  local shell="${SHELL:-}"
  [ -z "$shell" ] && return 1
  shell=$(basename "$shell")
  case "$shell" in
    bash|zsh|fish) echo "$shell"; return 0 ;;
    *) return 1 ;;
  esac
}

install_completions_for_shell() {
  local shell="$1"
  local bin="$DEST/$BINARY_NAME"
  if [ ! -x "$bin" ]; then
    warn "ags binary not found at $bin; skipping completions"
    return 1
  fi

  # Check if the completions subcommand exists
  if ! "$bin" completions --help >/dev/null 2>&1; then
    info "Shell completions: skipped (not supported in this version)"
    return 0
  fi

  local target=""
  case "$shell" in
    bash)
      target="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions/ags"
      ;;
    zsh)
      target="${XDG_DATA_HOME:-$HOME/.local/share}/zsh/site-functions/_ags"
      ;;
    fish)
      target="${XDG_CONFIG_HOME:-$HOME/.config}/fish/completions/ags.fish"
      ;;
    *)
      return 1
      ;;
  esac

  if ! mkdir -p "$(dirname "$target")" 2>/dev/null; then
    warn "Failed to create completions directory for $shell"
    return 1
  fi

  local output
  if output=$("$bin" completions "$shell" 2>&1) && [ -n "$output" ]; then
    printf '%s\n' "$output" > "$target"
    ok "Installed $shell completions to $target"
    return 0
  fi

  warn "Failed to generate $shell completions"
  return 1
}

maybe_install_completions() {
  local shell=""
  if ! shell=$(detect_default_shell); then
    info "Shell completions: skipped (unknown shell)"
    return 0
  fi

  install_completions_for_shell "$shell" || true
}

# ═══════════════════════════════════════════════════════════════════════════════
# Self-Test
# ═══════════════════════════════════════════════════════════════════════════════

run_self_test() {
  local bin="$DEST/$BINARY_NAME"
  if [ ! -x "$bin" ]; then
    err "Self-test: binary not found at $bin"
    return 1
  fi

  info "Running self-test..."

  # Test 1: --version
  local ver_output
  if ver_output=$("$bin" --version 2>&1); then
    ok "Self-test: --version works ($ver_output)"
  else
    err "Self-test: --version failed"
    return 1
  fi

  # Test 2: providers command
  if "$bin" providers >/dev/null 2>&1; then
    ok "Self-test: providers command works"
  else
    warn "Self-test: providers command returned non-zero (some providers may not be installed)"
  fi

  # Test 3: list command
  if "$bin" list --limit 1 >/dev/null 2>&1; then
    ok "Self-test: list command works"
  else
    warn "Self-test: list command returned non-zero (no sessions found, which is normal)"
  fi

  ok "Self-test complete"
}

# ═══════════════════════════════════════════════════════════════════════════════
# Usage
# ═══════════════════════════════════════════════════════════════════════════════

usage() {
  cat <<EOFU
Usage: install.sh [OPTIONS]

Options:
  --version vX.Y.Z   Install specific version (default: latest)
  --dest DIR         Install to DIR (default: ~/.local/bin)
  --system           Install to /usr/local/bin as the current non-root user
  --easy-mode        Auto-update PATH in shell rc files
  --yes              Non-interactive; auto-accept install prompts
  --verify           Run self-test after install
  --from-source      Build from source instead of downloading binary
  --quiet            Suppress non-error output
  --no-gum           Disable gum formatting even if available
  --no-verify        Skip checksum + signature verification (not recommended)
  --no-configure     Skip optional AGS setup
  --no-skill         Skip skill installation for Claude/Codex
  --identity FILE     Import an existing AGS age identity during initialization
  --offline TARBALL  Install from local tarball
  --force            Force reinstall even if same version is installed

Environment:
  VERSION            Override version to install
  ARTIFACT_URL       Override artifact download URL
  CHECKSUM           Override expected SHA256 checksum
  HTTPS_PROXY        HTTPS proxy URL
  HTTP_PROXY         HTTP proxy URL

Examples:
  # Install latest release
  curl -fsSL "https://raw.githubusercontent.com/jk-zhang-meta/ags/main/install.sh?\$(date +%s)" | bash

  # Install specific version with self-test
  bash install.sh --version v0.2.0 --verify

  # Install system-wide only when /usr/local/bin is already user-writable
  bash install.sh --system --easy-mode --yes

  # Offline / airgap install
  bash install.sh --offline ./ags-x86_64-unknown-linux-musl.tar.xz

  # Build from source (requires Rust nightly)
  bash install.sh --from-source

  # Skip optional setup
  bash install.sh --no-configure --no-skill
EOFU
}

# ═══════════════════════════════════════════════════════════════════════════════
# Argument Parsing
# ═══════════════════════════════════════════════════════════════════════════════

needs_arg() { if [ $# -lt 2 ] || [[ "$2" == --* ]]; then err "Missing value for $1"; usage; exit 1; fi; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version)      needs_arg "$@"; VERSION="$2"; shift 2 ;;
    --dest)         needs_arg "$@"; DEST="$2"; shift 2 ;;
    --system)       SYSTEM_INSTALL=1; DEST="/usr/local/bin"; shift ;;
    --easy-mode)    EASY=1; shift ;;
    --yes)          ASSUME_YES=1; shift ;;
    --verify)       VERIFY=1; shift ;;
    --artifact-url) needs_arg "$@"; ARTIFACT_URL="$2"; shift 2 ;;
    --checksum)     needs_arg "$@"; CHECKSUM="$2"; shift 2 ;;
    --checksum-url) needs_arg "$@"; CHECKSUM_URL="$2"; shift 2 ;;
    --from-source)  FROM_SOURCE=1; shift ;;
    --quiet|-q)     QUIET=1; shift ;;
    --no-gum)       NO_GUM=1; shift ;;
    --no-verify)    NO_CHECKSUM=1; shift ;;
    --no-configure) NO_CONFIGURE=1; shift ;;
    --no-skill)     NO_SKILL=1; shift ;;
    --identity)     needs_arg "$@"; CHECKPOINT_IDENTITY="$2"; shift 2 ;;
    --force)        FORCE_INSTALL=1; shift ;;
    --offline)      needs_arg "$@"; OFFLINE_TARBALL="$2"; shift 2 ;;
    -h|--help)      usage; exit 0 ;;
    *)
      err "Unknown option: $1"
      usage
      exit 1
      ;;
  esac
done

if [ "$SYSTEM_INSTALL" -eq 1 ] && [ "$EUID" -eq 0 ]; then
  err "--system cannot run as root because AGS configuration belongs to the target user"
  err "Run the installer as that user with a writable --dest (the default is recommended)"
  exit 1
fi

if [ -n "$CHECKPOINT_IDENTITY" ]; then
  case "$CHECKPOINT_IDENTITY" in
    /*) ;;
    *) err "--identity requires an absolute file path"; exit 1 ;;
  esac
  [ -r "$CHECKPOINT_IDENTITY" ] ||
    { err "Identity file is not readable: $CHECKPOINT_IDENTITY"; exit 1; }
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Main Installation Flow
# ═══════════════════════════════════════════════════════════════════════════════

# Recover an interrupted binary transaction before network resolution or any
# new installation work.
mkdir -p "$DEST" || {
  err "Cannot create destination directory: $DEST"
  exit 1
}
DEST="$(cd "$DEST" && pwd -P)" || {
  err "Cannot resolve destination directory: $DEST"
  exit 1
}
INSTALL_TRANSACTION_FILE="$DEST/.${BINARY_NAME}.install-transaction.json"
preflight_installer_tools

# ═══════════════════════════════════════════════════════════════════════════════
# Atomic Locking (mkdir-based, cross-platform)
# ═══════════════════════════════════════════════════════════════════════════════

LOCK_DIR="${LOCK_FILE}.d"
LOCKED=0
release_lock_dir() {
  rm -f "$LOCK_DIR/pid" 2>/dev/null || true
  rmdir "$LOCK_DIR" 2>/dev/null || true
}

if mkdir "$LOCK_DIR" 2>/dev/null; then
  LOCKED=1
  echo $$ > "$LOCK_DIR/pid"
else
  if [ -f "$LOCK_DIR/pid" ]; then
    OLD_PID=$(cat "$LOCK_DIR/pid" 2>/dev/null || echo "")
    if [ -n "$OLD_PID" ] && ! kill -0 "$OLD_PID" 2>/dev/null; then
      release_lock_dir
      if mkdir "$LOCK_DIR" 2>/dev/null; then
        LOCKED=1
        echo $$ > "$LOCK_DIR/pid"
      fi
    fi
  fi
  if [ "$LOCKED" -eq 0 ]; then
    err "Another ags installer is running (lock $LOCK_DIR)"
    exit 1
  fi
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Temp Directory & Cleanup Trap
# ═══════════════════════════════════════════════════════════════════════════════

TMP=$(mktemp -d)
cleanup() {
  local status=$?
  if [ "$status" -ne 0 ] && [ "$INSTALL_CORE_COMMITTED" -eq 0 ] &&
     [ "$INSTALL_TRANSACTION_ACTIVE" -eq 1 ] &&
     ! restore_install_transaction_if_safe; then
    warn "Installer transaction was retained because automatic rollback was not proven safe"
  fi
  [ -z "$BINARY_STAGE" ] || rm -f -- "$BINARY_STAGE" 2>/dev/null || true
  rm -rf "$TMP" 2>/dev/null || true
  if [ "$LOCKED" -eq 1 ]; then
    release_lock_dir
  fi
}
trap cleanup EXIT

recover_pending_install_transaction || exit 1
if [ "$INSTALL_TRANSACTION_ACTIVE" -eq 1 ]; then
  # The installed ags is already the exact journaled candidate. Finish Context
  # Mode and commit that transaction without requiring the original artifact or
  # any network access.
  RESUME_CORE_ONLY=1
fi

# Show branded header
if [ "$QUIET" -eq 0 ]; then
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    gum style \
      --border normal \
      --border-foreground 39 \
      --padding "0 1" \
      --margin "1 0" \
      "$(gum style --foreground 42 --bold 'ags installer')" \
      "$(gum style --foreground 245 'Cross Agent Session Resumer')" \
      "$(gum style --foreground 245 'Resume AI coding sessions across providers')"
  else
    echo ""
    echo -e "\033[1;32mags installer\033[0m"
    echo -e "\033[0;90mCross Agent Session Resumer\033[0m"
    echo -e "\033[0;90mResume AI coding sessions across providers\033[0m"
    echo ""
  fi
fi

# Detect providers early (informational display)
print_provider_scan_notice
detect_providers
if [ "$QUIET" -eq 0 ]; then
  print_detected_providers
fi

# Setup proxy
setup_proxy

if [ "$RESUME_CORE_ONLY" -eq 1 ]; then
  info "Finishing the recovered ags transaction"
else
  # Resolve version and platform only for new installation work. A recovered
  # candidate is already identified by its journaled SHA-256.
  resolve_version
  detect_platform
  set_artifact_url
fi

# Preflight
preflight_checks

# Check if already at target version.
# Keep post-install steps idempotent so installer still refreshes local setup.
INSTALLED_AGS_VERSION=""
if [ "$RESUME_CORE_ONLY" -eq 0 ] && [ "$FORCE_INSTALL" -eq 0 ] &&
   check_installed_version "$VERSION"; then
  ok "ags $INSTALLED_AGS_VERSION is already installed at $DEST/$BINARY_NAME (target $VERSION)"
  info "Use --force to reinstall"
  INSTALL_SOURCE="already installed ($INSTALLED_AGS_VERSION)"
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Offline Install Path
# ═══════════════════════════════════════════════════════════════════════════════

INSTALL_SOURCE="${INSTALL_SOURCE:-}"
if [ "$INSTALL_TRANSACTION_ACTIVE" -eq 1 ]; then
  INSTALL_SOURCE="recovered interrupted binary activation"
fi

if [ -n "$OFFLINE_TARBALL" ] && [ "$RESUME_CORE_ONLY" -eq 0 ]; then
  if [ ! -f "$OFFLINE_TARBALL" ]; then
    err "Offline tarball not found: $OFFLINE_TARBALL"
    exit 1
  fi
  info "Installing from offline tarball: $OFFLINE_TARBALL"
  cp "$OFFLINE_TARBALL" "$TMP/artifact.tar.xz"
  tar -xf "$TMP/artifact.tar.xz" -C "$TMP"

  if [ "$INSTALL_TRANSACTION_ACTIVE" -eq 0 ]; then
    BIN="$TMP/$BINARY_NAME"
    if [ ! -x "$BIN" ] && [ -n "$TARGET" ]; then
      BIN="$TMP/ags-${TARGET}/$BINARY_NAME"
    fi
    if [ ! -x "$BIN" ]; then
      BIN=$(find "$TMP" -maxdepth 3 -type f -name "$BINARY_NAME" -perm -111 | head -n 1)
    fi
    [ -x "$BIN" ] || { err "Binary not found in tarball"; exit 1; }

    artifact_version="$("$BIN" --version 2>/dev/null | head -1 |
      sed -E 's/[^0-9]*([0-9]+\.[0-9]+\.[0-9]+).*/\1/')"
    [[ "$artifact_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
      err "Cannot determine the version of the offline binary"
      exit 1
    }
    if [ -n "$VERSION" ]; then
      requested_core="${VERSION#v}"
      requested_core="${requested_core%%[-+]*}"
      if [ "$requested_core" != "$artifact_version" ]; then
        err "Offline artifact is v$artifact_version, not requested $VERSION"
        exit 1
      fi
    else
      VERSION="v$artifact_version"
    fi
    install_ags_binary "$BIN"
    ok "Installed to $DEST/$BINARY_NAME (offline)"
    INSTALL_SOURCE="offline tarball"
  else
    info "Keeping the exact ags candidate from the recovered transaction"
  fi
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Download Binary (with build-from-source fallback)
# ═══════════════════════════════════════════════════════════════════════════════

if [ -z "$INSTALL_SOURCE" ] && [ "$FROM_SOURCE" -eq 0 ] && [ -n "$URL" ]; then
  info "Downloading $URL"
  DOWNLOAD_OK=0
  if run_with_spinner "Downloading ags..." \
    curl -fsSL "${PROXY_ARGS[@]}" "$URL" -o "$TMP/$TAR"; then
    DOWNLOAD_OK=1
  fi

  if [ "$DOWNLOAD_OK" -eq 0 ]; then
    # Tier 2: unversioned latest
    TIER2_URL="https://github.com/${OWNER}/${REPO}/releases/latest/download/ags-${TARGET}.tar.xz"
    info "Trying unversioned latest: $TIER2_URL"
    if curl -fsSL "${PROXY_ARGS[@]}" "$TIER2_URL" -o "$TMP/$TAR" 2>/dev/null; then
      DOWNLOAD_OK=1
    fi
  fi

  if [ "$DOWNLOAD_OK" -eq 0 ]; then
    # Tier 3: simple naming
    TIER3_URL="https://github.com/${OWNER}/${REPO}/releases/latest/download/ags-${OS}-${ARCH}.tar.xz"
    info "Trying simple naming: $TIER3_URL"
    if curl -fsSL "${PROXY_ARGS[@]}" "$TIER3_URL" -o "$TMP/$TAR" 2>/dev/null; then
      DOWNLOAD_OK=1
    fi
  fi

  if [ "$DOWNLOAD_OK" -eq 0 ]; then
    warn "No prebuilt binary found; falling back to build-from-source"
    FROM_SOURCE=1
  fi
fi

if [ -z "$INSTALL_SOURCE" ] && [ "$FROM_SOURCE" -eq 1 ]; then
  info "Building from source (requires git, Rust nightly)"
  ensure_rust
  run_with_spinner "Cloning repository..." \
    git clone --depth 1 "https://github.com/${OWNER}/${REPO}.git" "$TMP/src"
  BUILD_TARGET_DIR="$TMP/src/target"
  run_with_spinner "Building from source (this takes a few minutes)..." \
    bash -c "cd \"\$1\" && CARGO_TARGET_DIR=\"\$2\" cargo build --release --bin \"\$3\"" \
      _ "$TMP/src" "$BUILD_TARGET_DIR" "$BINARY_NAME"
  BIN="$BUILD_TARGET_DIR/release/$BINARY_NAME"
  [ -x "$BIN" ] || { err "Build failed: binary not found at $BIN"; exit 1; }
  install_ags_binary "$BIN"
  ok "Installed to $DEST/$BINARY_NAME (source build)"
  INSTALL_SOURCE="built from source (Rust nightly)"
fi

# Binary download path (not offline, not from-source)
if [ -z "$INSTALL_SOURCE" ]; then
  # ═════════════════════════════════════════════════════════════════════════════
  # Verify Downloaded Artifact
  # ═════════════════════════════════════════════════════════════════════════════

  if [ "$NO_CHECKSUM" -eq 1 ]; then
    warn "Verification skipped (--no-verify)"
  else
    # Fetch checksum
    if [ -z "$CHECKSUM" ]; then
      [ -z "$CHECKSUM_URL" ] && CHECKSUM_URL="${URL}.sha256"
      info "Fetching checksum from ${CHECKSUM_URL}"
      CHECKSUM_FILE="$TMP/checksum.sha256"
      if curl -fsSL "${PROXY_ARGS[@]}" "$CHECKSUM_URL" -o "$CHECKSUM_FILE" 2>/dev/null; then
        CHECKSUM=$(awk '{print $1}' "$CHECKSUM_FILE")
        if [ -z "$CHECKSUM" ]; then
          warn "Empty checksum file; skipping verification"
        fi
      else
        warn "Checksum file not found; skipping checksum verification"
      fi
    fi

    # Verify checksum if available
    if [ -n "$CHECKSUM" ]; then
      if ! verify_checksum "$TMP/$TAR" "$CHECKSUM"; then
        err "Installation aborted due to checksum failure"
        exit 1
      fi
    fi

    # Verify sigstore bundle (best-effort)
    if ! verify_sigstore_bundle "$TMP/$TAR" "$URL"; then
      err "Signature verification failed"
      err "The downloaded file may be corrupted or tampered with."
      exit 1
    fi
  fi

  # ═════════════════════════════════════════════════════════════════════════════
  # Extract & Install Binary
  # ═════════════════════════════════════════════════════════════════════════════

  info "Extracting"
  tar -xf "$TMP/$TAR" -C "$TMP"

  BIN="$TMP/$BINARY_NAME"
  if [ ! -x "$BIN" ] && [ -n "$TARGET" ]; then
    BIN="$TMP/ags-${TARGET}/$BINARY_NAME"
  fi
  if [ ! -x "$BIN" ]; then
    BIN=$(find "$TMP" -maxdepth 3 -type f -name "$BINARY_NAME" -perm -111 | head -n 1)
  fi
  [ -x "$BIN" ] || { err "Binary not found in archive"; exit 1; }

  install_ags_binary "$BIN"
  ok "Installed to $DEST/$BINARY_NAME"
  INSTALL_SOURCE="prebuilt binary ($VERSION)"
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Post-Install (shared across all install paths)
# ═══════════════════════════════════════════════════════════════════════════════

if [ "$INSTALL_TRANSACTION_ACTIVE" -eq 1 ]; then
  commit_install_transaction || {
    err "The binary transaction could not be committed"
    exit 1
  }
fi
INSTALL_CORE_COMMITTED=1
configure_codext_only
maybe_add_path
maybe_install_completions
configure_agents

if [ "$VERIFY" -eq 1 ]; then
  run_self_test
fi

# ═══════════════════════════════════════════════════════════════════════════════
# Final Summary
# ═══════════════════════════════════════════════════════════════════════════════

PROV_LIST=""
if [[ ${#DETECTED_PROVIDERS[@]} -gt 0 ]]; then
  for p in "${DETECTED_PROVIDERS[@]}"; do
    case "$p" in
      claude-code) PROV_LIST+="cc " ;;
      codex)       PROV_LIST+="cod " ;;
      gemini)      PROV_LIST+="gmi " ;;
      cursor)      PROV_LIST+="cur " ;;
      cline)       PROV_LIST+="cln " ;;
      aider)       PROV_LIST+="aid " ;;
      amp)         PROV_LIST+="amp " ;;
      opencode)    PROV_LIST+="opc " ;;
      chatgpt)     PROV_LIST+="gpt " ;;
    esac
  done
  PROV_LIST="${PROV_LIST% }"
else
  PROV_LIST="none detected"
fi

summary_lines=(
  "Binary:           $DEST/$BINARY_NAME"
  "Version:          $VERSION"
  "Install source:   $INSTALL_SOURCE"
  "Providers:        $PROV_LIST"
  "Skill source:     $SKILL_ARCHIVE_STATUS"
  "Claude skill:     $CLAUDE_SKILL_STATUS"
  "Codex skill:      $CODEX_SKILL_STATUS"
  "Wrapper cc:       $CC_WRAPPER_STATUS"
  "Wrapper cod:      $COD_WRAPPER_STATUS"
  "Wrapper gmi:      $GMI_WRAPPER_STATUS"
  "Wrapper ags:      $AGS_WRAPPER_STATUS"
  "AGS Codex skill:  $AGS_CODEX_SKILL_STATUS"
  "AGS Claude skill: $AGS_CLAUDE_SKILL_STATUS"
  "AGS hooks:        $AGS_HOOK_STATUS"
  "AGS vault:        $AGS_INIT_STATUS"
  "codext:           $CODEXT_STATUS"
  ""
  "Get started:"
  "  ags convert providers"
  "  ags convert list"
  "  ags convert -cc <session-id>"
  "  ags convert -cod <session-id>"
  "  ags convert -gmi <session-id>"
  "  ags ls"
  "  ags"
  ""
  "Managed paths:"
  "  binary:   $(status_path "$DEST/$BINARY_NAME")"
  "  wrappers: $(status_path "$DEST")/{cc,cod,gmi,ags}"
  "  skills:   ~/.claude/skills/{ags,ags-convert} and ~/.codex/skills/{ags,ags-convert}"
)

echo ""

if [ "$QUIET" -eq 0 ]; then
  if [ "$HAS_GUM" -eq 1 ] && [ "$NO_GUM" -eq 0 ]; then
    {
      gum style --foreground 42 --bold 'ags installed successfully!'
      echo ""
      for line in "${summary_lines[@]}"; do
        if [ -z "$line" ]; then
          echo ""
          continue
        fi
        if [[ "$line" == "Get started:" ]] || [[ "$line" == "Managed paths:" ]]; then
          gum style --foreground 245 "$line"
        elif [[ "$line" == "  ags "* ]]; then
          gum style --foreground 39 "$line"
        else
          gum style --foreground 245 "$line"
        fi
      done
    } | gum style --border normal --border-foreground 42 --padding "1 2"
  else
    box_lines=("\033[1;32mags installed successfully!\033[0m" "")
    for line in "${summary_lines[@]}"; do
      box_lines+=("$line")
    done
    draw_box "0;32" "${box_lines[@]}"
  fi
fi
