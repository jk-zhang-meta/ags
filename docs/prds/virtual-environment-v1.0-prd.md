# AGS Virtual Environment v1.0

状态：设计草案（基于 Codex 源码审计、Claude Code 2.1.258 逆向证据和当前 AGS 边界）

## 1. 目标

AGS 为 agent 提供一个可验证的目标操作系统环境。agent 执行命令时，实际命令在选定的 Linux、WSL2、Linux VPS 或 macOS 后端运行；agent 通过命令、环境变量、标准库、文件、`/proc`、网络和终端观察到的事实，应与目标 profile 一致，并且不因后端是 WSL、VPS 或 AGS 兼容层而得到额外线索。

本版本假定 agent 不会恶意进行旁路攻击。仍然要求常规 OS 探测不泄露载体，但不把兼容层描述成能够抵抗任意恶意 native 程序的安全边界。

## 2. 非目标

- 不把所有命令替换成预先录制的文本。
- 不把 WSL 当作 Windows agent；WSL 的执行语义保持 Linux。
- 不承诺在未运行目标内核或 VM 时，对任意 syscall、静态二进制和设备探测都不可区分。
- 不改变 agent 的业务协议或提示词来掩盖未实现的 OS 事实。

## 3. 术语与模型

### 3.1 Execution backend

命令实际执行的地方：`linux-native`、`wsl2`、`linux-vps`、`macos-native`。backend 提供真实进程、文件、网络和 PTY 能力。

### 3.2 Environment profile

agent 应看到的环境契约。profile 包含 OS/发行版、内核呈现、hostname、用户和组、路径根、时区和 tzdata、locale、资源、网络、工具版本及允许的观测例外。

### 3.3 Overlay

将 backend 的真实结果转换为 profile 结果的适配层。Overlay 只能覆盖声明的观察面；未覆盖的事实状态必须是 `host_visible` 或 `unknown`，不能默认为虚拟成功。

### 3.4 可信度

“可信”定义为 `environment-contract-v1` 的 probe 集合通过，且所有必须虚拟化的观察面没有 `host_visible` 泄露。它是有边界的观察等价性，不是对恶意代码的隐身保证。

## 4. 平台规则

| backend | 平台身份 | 必须保留 | 必须隐藏或统一 |
|---|---|---|---|
| linux-native | Linux | Linux 用户空间、内核和工具链 | profile 指定的 hostname、用户、路径、时区、资源 |
| wsl2 | Linux | Linux 命令、Linux 路径和进程语义 | `WSL_*`、Microsoft kernel 文本、DrvFs/Windows 挂载、Windows 互操作入口、宿主路径 |
| linux-vps | Linux | VPS 上真实 Linux 行为 | 云厂商/宿主节点/虚拟化 provenance、非 profile 的主机身份 |
| macos-native | macOS | Darwin 用户空间、macOS 工具和 PTY | profile 指定的机器身份、路径、时区和资源 |

WSL 的虚拟机来源记录在 AGS 自己的审计日志中，不进入 agent 可见环境。VPS 的云基础设施来源同理。

## 5. 用户配置草案

```yaml
profile: linux-ubuntu-24
backend:
  kind: wsl2                 # linux-native | wsl2 | linux-vps | macos-native
  endpoint: default          # VPS/Mac 使用受信远程 endpoint
contract: environment-contract-v1
identity:
  hostname: build-node-01
  username: agent
  uid: 1000
  gid: 1000
  groups: [agent]
time:
  timezone: Asia/Shanghai
  tzdata: backend             # 或 pinned artifact
locale:
  lang: en_US.UTF-8
paths:
  workspace: /workspace/project
  home: /home/agent
  tmp: /tmp
policy:
  unimplemented_probe: fail_closed
  host_visible: []
```

profile 可以来自参考主机的采样，也可以完全声明。AGS 必须保存 profile 摘要和 probe 版本，保证同一会话重启后契约不漂移。

## 6. Runtime 接口

AGS 应引入一个深接口 `EnvironmentRuntime`，而不是继续向 `LaunchSpec` 添加零散伪装字段：

```text
describe() -> BackendFacts
exec(command, cwd, env, tty) -> ProcessHandle
filesystem() -> FileView
clock() -> ClockView
identity() -> IdentityView
network() -> NetworkView
probe(probe_id) -> ProbeResult
```

适配器为 `LinuxBackend`、`WslBackend`、`VpsBackend` 和 `MacosBackend`；`ProfileOverlay` 在各 view 上统一结果。`LaunchSpec` 只保留程序、参数、工作目录、显式环境和目标选择，runtime 负责执行位置及可见事实。

## 7. 必须覆盖的观察面

### 7.1 时间与时区

同时建模 realtime、monotonic、monotonic raw、boottime、CPU time、sleep/timerfd/epoll deadline、文件 mtime、HTTP `Date`、证书有效期、DST 和 tzdata 版本。`TZ`、`/etc/localtime`、glibc、ICU/Bun、Python zoneinfo 和 Rust API 必须使用同一 profile 规则。

Codex 的 `TimeProvider` 只覆盖部分路径；`SystemTime::now`、`Utc::now`、`Instant::now` 的直接调用必须列入审计。Claude 同时使用运行时本地时区、UI/调度时区和 UTC 持久化时间，三者不能混为一个字段。

### 7.2 身份与平台

覆盖 uid/gid/groups、passwd、capabilities、hostname、`uname`、`/etc/os-release`、arch、`/proc/self/exe`、PID/PPID、进程树、cgroup、rlimit、CPU affinity、`sysconf`、`sysinfo`、`statvfs` 和挂载信息。

### 7.3 文件与路径

覆盖逻辑/物理 cwd、`PWD`、`/proc/self/cwd`、symlink、`openat`/dirfd、相对路径、权限、错误消息路径、home/tmp/workspace 和 `argv[0]`。WSL 的 `/mnt/*` 和 Windows 路径必须按 profile 决定是否可见。

### 7.4 网络与终端

覆盖 DNS、IPv4/IPv6、代理、TLS/SNI/CA、HTTP Date、loopback、PTY、`isatty`、窗口大小、termios、SIGWINCH、控制终端和信号行为。子进程、fork/exec、MCP/plugin 必须继承同一 runtime 规则。

### 7.5 环境变量与 locale

统一 `LANG`、`LC_ALL`、`LC_TIME`、`TZ`、代理变量和临时目录。不能只依赖当前 AGS 的启动 env 注入；Codex Unix 默认 env 集合不含 `TZ`，Claude/Bun 还会直接读取 ICU 和系统时区数据。

## 8. WSL 专项要求

WSL 后端必须通过 probe 检查并处理：`WSL_DISTRO_NAME`、`WSL_INTEROP`、`/proc/version` 的 `microsoft|wsl`、DrvFs 类型、`/mnt/c`、Windows 命令入口、PID 1、cgroup、挂载、设备、DNS、代理、`/proc/self/exe` 和错误信息中的宿主路径。任何一项被标为必须隐藏却仍为 `host_visible`，profile 必须失败关闭。

## 9. Probe 与状态

每个 probe 记录 id、scope、期望值、容差、版本、后端实测值、状态和失败策略。状态只能是：

- `virtual`：结果由 profile 控制并通过一致性检查；
- `host_visible`：结果来自后端，可能暴露载体；
- `unknown`：AGS 没有可靠证据。

`probe-set-v1` 至少包含时间、locale、身份、platform、proc、fs、net、tty、子进程继承和未实现探测的负面用例。报告只有在所有 required probe 为 `virtual` 且没有禁止的 `host_visible` 时才显示可信通过。

## 10. 分阶段交付

1. **Profile/contract**：定义 schema、版本、状态和审计记录。
2. **真实 backend**：先完成本机 Linux/WSL，再接入 Linux VPS 和 macOS remote backend。
3. **低风险 overlay**：环境变量、cwd/home/tmp、hostname、locale、TZ 和工具路径。
4. **观察面适配**：proc/sys、uname、路径、PTY、网络和资源。
5. **时间统一**：realtime/monotonic/sleep/tzdata 的注入与直接调用审计。
6. **Codex/Claude conformance**：用无凭据、无破坏性的 probe 运行真实启动链和子进程继承测试。
7. **失败关闭与回归**：未覆盖观察面不得宣称通过；记录 backend、profile、probe 和版本摘要。

## 11. 验收标准

- 同一 profile 在 Linux 原生、WSL2、Linux VPS（可用时）和 macOS 后端生成稳定的 contract 报告。
- agent 通过常规 shell、Node/Bun、Python、Rust 和 `/proc` 探测时得到一致目标结果；WSL/VPS provenance 不出现在 required probe 中。
- 时区跨午夜、DST 前后和不同进程（含 MCP/plugin）结果一致。
- 未实现 probe 被准确报告为 `unknown`/`host_visible`，不会被汇总为成功。
- backend 断开、profile 不完整或结果冲突时，AGS 拒绝启动或明确降级，不静默伪造。
- 不改变现有 session resume 数据；新增 runtime 信息可追溯到 profile、backend 和 probe 版本。

## 12. GPT-5.6-Sol 复核后的强制修订

两轮独立复核一致认为：研究方向正确，但当前实现不能直接承诺“任意探测下完全隐身”。以下规则在进入编码前必须加入 runtime 设计。

### 12.1 两个正交维度

目标系统与载体来源必须分开：

- `target_os`：agent 应看到的 Linux 或 macOS；
- `carrier_provenance`：native、WSL2、VPS 或 remote Mac，只进入 AGS 内部审计。

`carrier_provenance`、真实 endpoint、宿主 hostname 和宿主路径禁止进入 provider prompt、`EnvSnapshot`、agent-facing config、错误文本和普通日志。启动前必须有 data-flow redaction test。

### 12.2 能力矩阵与启动门槛

每条 required probe 必须声明 mechanism 和允许来源。来源至少包括：

`native_match`、`overlay_virtual`、`brokered`、`host_visible`、`unknown`、`mismatch`、`error`。

同值的真实结果是 `native_match`，不能标作 `virtual`。如果 required probe 没有可执行机制，或结果是 `host_visible/mismatch/error`，启动必须拒绝；只有 profile 明确允许的 optional probe 才能降级为 `degraded`。contract 总状态只能是 `equivalent`、`degraded` 或 `blocked`。

这条门槛尤其适用于 `uname`、`/proc/version`、UID/GID、PID/PPID、DMI/sysfs、挂载、CPU、boot/monotonic clock 和静态/直接 syscall 程序。bwrap、环境变量和命令文本重写不能宣称覆盖这些面；需要真正的目标远端、VM 或 syscall/runtime broker。

### 12.3 Profile 必须绑定不可变事实

除示例字段外，profile 还必须包含 target image/rootfs digest 或 remote target reference、kernel/arch/ABI contract、工具链和动态库版本、passwd/shell/umask/capabilities、proc/sys/mount、资源、网络、PTY、PATH 映射，以及 pinned tzdata/ICU/locale artifact 的版本和 hash。`tzdata: backend` 只允许在 profile 明确接受 backend 差异时使用。

### 12.4 WSL、VPS 和 macOS 的硬性探针

- **WSL**：`WSL_*`、`/proc/version`、`/init`、`/usr/lib/wsl`、`/run/WSL`、DrvFs/9p/Plan9、`/mnt/wsl*`、Windows 命令入口、PID1/cgroup/namespace、自动生成的 resolv.conf、Windows 路径和错误信息。
- **VPS**：`169.254.169.254` metadata、cloud-init、DMI/ACPI/virtio/KVM/Xen、provider hostname/DNS/CA、guest agent、块设备序列号、出口 IP/路由/接口。
- **macOS**：Darwin kernel/build、`sysctl`、`sw_vers`、`ioreg`/hardware model、arm64/x86_64/Rosetta、launchd、PTY、SSH transport 和远端 capability handshake。Linux wrapper 不得冒充 Darwin 原生 API。

### 12.5 可执行验收矩阵

验收矩阵为 `backend × runtime(shell/Node/Bun/Python/Rust/C-static) × process-depth(parent/fork/exec/grandchild/MCP) × time-fixture(normal/midnight/DST)`。每次报告保存 profile hash、backend capability hash、probe-set 版本、证据 hash、采样时间、origin 和错误/降级状态。

必须包含 negative leak tests：WSL/VPS 标识、`/mnt/c`、DrvFs/9p、powershell/cmd、DMI/virt markers、machine-id、`/proc/self/exe/cwd/mountinfo`、cloud metadata、宿主 IP/路由/hostname、继承的 XDG/SSH/PATH、宿主 TZ/locale/UID/PID/PTY。子进程逃逸、MCP/plugin 失去 overlay、远程断开和 unknown probe 都必须使状态变为 `degraded` 或 `blocked`，不能静默继续。

### 12.6 三档产品承诺

AGS 对外文档应明确三档能力：

1. **A：真实后端身份**——不隐藏载体，只保证命令在指定平台运行。
2. **B：命名 probe 集合上的观察等价**——本版本的目标，适用于不恶意 agent，要求 capability gate 和失败关闭。
3. **C：系统级不可区分/安全隔离**——需要目标 VM、真实远端内核或同等级执行边界；不能用 B 档的结果代替。

因此 v1.0 的“可信”固定表述为：**在 `environment-contract-v1` 明确列出的 probe 集合上达到 observational equivalence**。不得宣称对任意 native 程序、任意 syscall 或任意硬件探测都不可区分。
