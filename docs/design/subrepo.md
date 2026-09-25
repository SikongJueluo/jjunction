# Sub-repo 管理（决策记录）

- 日期：2026-09-23
- 状态：已接受
- 前置调研：`docs/research/git-operations-stack.md`（gix vs git 子进程；勘误见其第 4 节注记）

## 背景

jjunction 需要把外部仓库（第三方库、姊妹项目）物化进 workspace，语义参考 Zephyr west
的 manifest + update，但只取其核心：**manifest 记意图、lock 记确定状态**。west 的
import/groups/extension/topology 一概不要。

## 决策

### D1 sub-repo 形态：readonly vendored plain git checkout

- 物化产物是普通 git 仓，HEAD detach 在锁定 commit 上；**永不** jj colocate
  （要开发就 fork——fork 只是 `[[repo]]` 里换 `url` + `rev`，零额外机制）
- readonly 是**被动**的：doctor 警告脏工作树，apply 跳过脏仓；不做 chmod / hooks 等主动保护
- 状态等式唯一：`HEAD == lock.commit`。不存在 jj working-copy 启发式

### D2 manifest 与 lock

manifest：`.jjunction/config.toml` 内 `[[repo]]` 数组（用户手写，`repo add` 亦可写）：

```toml
[[repo]]
name   = "habitat-sim"                        # 缺省 = url basename 去 .git
url    = "https://github.com/…/habitat-sim.git"
target = "third_party/habitat-sim"            # 缺省 = name，相对 workspace 根
rev    = "main"                               # 缺省 = 浮动（跟随 origin/HEAD）
```

lock：`.jjunction/lock.toml`，机器生成、默认 tracked（团队复现靠它）：

```toml
version = 1

[repo.habitat-sim]
commit = "3f9d2c1e…"
```

### D3 lock 最小 diff 纪律（目标：diff 只含语义变化）

1. 表结构而非数组：`[repo.<name>]` 每仓独立块，增删改不 reflow 全文件
2. 原位修改：toml_edit load → 只改变化的 key → save；无变化则字节不动
3. 最小字段：每仓仅 `commit`；url/rev 只在 manifest；不存 fetched-at 等易变元数据
4. `version` 为常量，仅格式 break 时递增；LF、EOF 换行、裸 key 固定
5. 首次写入按 name 定序，此后永不重排

### D4 rev 语义

- `rev` 缺省 = 浮动：解析为本地 `refs/remotes/origin/HEAD`；缺失时 fallback
  `git ls-remote --symref <url> HEAD`
- branch / tag / SHA 三态统一在**本地**解析（gix in-process）：SHA 直读、
  branch → `refs/remotes/origin/<rev>`、tag → `refs/tags/<rev>`（peel 到 commit）
- 按 SHA fetch 依赖服务端 `uploadpack.allow*SHA1InWant`；降级链：
  本地已有 → `fetch origin <sha>` → 全量 `+refs/heads/*` → 报错并解释

### D5 git 操作栈：gix 管"看"，git 子进程管"动"

- **本地读**走 gix 0.87 in-process（已在依赖树，core API 零 feature 门槛）：
  HEAD、remote refs、peel。jj-lib 不进 sub-repo 路径（打开 git 仓必然 jj 化：
  写 `.jj/`、op log、snapshot，全是否决项）
- **网络与写**走 git 子进程：`clone --no-checkout`、`fetch`、`checkout --detach`
  （脏树自动拒绝 = 天然安全闸）、`status --porcelain`（doctor 用）
- 依据：jj-lib 0.45 自身的 fetch/push 就是 git 子进程包装；树内无 in-process
  网络路径；子进程吃满用户 gitconfig（代理 / insteadOf 镜像，正确性刚需）
- 版本下限 ≈ 任意现代 git；不解析 `fetch --porcelain`（否则对齐 jj 的 2.41）

### D6 CLI：并入两动词模型 + manifest 编辑动词

- `jjn apply`：新增 repo 阶段，顺序 **repos → links → workspace**（link 可能指向
  repo 内路径）。必要时网络（新机首 clone 一次），**不设 auto-sync 键**——
  网络只发生在"仓缺失 / lock 领先本地对象"两种罕见态，常态全离线
- `jjn doctor`：扩展 repo 健康检查（见 D8）
- `jjn repo add <url> [--target] [--rev] [--name]`：toml_edit 追加 `[[repo]]`
  （保留注释）+ 物化 + 写 lock
- `jjn repo remove <name>`：删 manifest 与 lock 条目；**不删磁盘目录**（只解除管理）
- `jjn repo update [name…]`：fetch + 重解析 rev → 原位写 lock → checkout

### D7 trust：与 `[[link]]` 同门

`[[repo]]` 触发外部 clone（供应链面），未列入全局 `trusted-repos` 时整个忽略并
hint，不另设"SHA 才许自动同步"的中间门。

### D8 状态判定与 doctor

| 检查 | 判定 |
| --- | --- |
| 未物化 | target 不存在 |
| 不同步 | `HEAD != lock.commit` |
| 脏 | `git status --porcelain` 非空 |
| lock 缺条目 | 有 manifest 条目无 lock 条目（apply 会补） |
| lock 漂移 | manifest `rev` 为完整 SHA 且 ≠ `lock.commit`（廉价直检；浮动 rev 不检，接受 origin/HEAD 陈旧） |
| 重复声明 | name / target 冲突 |

### D9 代码结构

| 文件 | 职责 |
| --- | --- |
| `src/repo.rs` | `RepoEntry` 模型、`load_entries`、物化 / update / doctor（沿用状态机式 Status 枚举 + 纯函数 + tempfile 单测） |
| `src/lock.rs` | lock 读写：toml_edit 原位、D3 纪律、最小 diff 单测 |
| `src/bin/jjn.rs` | apply 加 repo 阶段；doctor 扩展；`repo add/remove/update` 子命令 |

git 子进程封装与 gix 读放 `src/repo.rs` 内（量小不拆模块）。

## 后果与风险

- 直接依赖 `gix = "0.87"`（caret）：jj-lib 升级 bump gix 后短期双版本共存（多编译、
  不出错），需跟进同步；属机械维护
- git 子进程要求 PATH 有 git（与 jj 0.30+ 网络面同前提，非新增负担）
- lock 冲突：hunks 不相交可自动 merge；同仓双改需手解，不提供 merge 工具
- enterShell 后台 `apply` 首次进新 workspace 会后台 clone 大仓（一次性，接受）；
  后手 `apply --offline`
- 浮动 rev 的远端默认分支变更无法廉价检测（接受；`repo update` 主动刷新）
- Windows 延后（路径与 symlink 语义）

## 变更（2026-09-25，secondary workspace 共享）

jj 多 workspace 下 sub-repo 之前只物化在默认工作区，secondary 里 `deps/` 缺失。
修正：`jjn apply` 从任何 workspace 运行都收敛整个拓扑——

- 默认 workspace 永远持有规范 checkout（clone + checkout 到 lock）
- `[repos] secondary = "link"`（默认）：所有 secondary 的 `<target>` 以**绝对路径
  symlink** 指向默认工作区对应目录；默认侧未物化时不建链（防 dangling）
- `secondary = "clone"`：仅调用者所在 workspace 独立 clone（旧行为）；
  `"skip"`：secondary 不物化
- doctor 在 link 模式下于 secondary 中追加 `[secondary]` 链接健康检查
- 各 workspace 根路径需各自通过 trust（与 direnv allow 每目录一放行同构）

## 变更（2026-09-24，可用性返工）

真实使用暴露的连环坑（trust 无入口、repo add 非原子、无可见性命令、空目录语义
偏离 git）催生以下修正：

- 新增 `jjn trust`：direnv-allow 式信任入口；所有 untrusted 提示指向它
- trust 门现在覆盖一切物化动词（apply / sync / update / add）；纯 manifest
  手术（remove / list）不设门，作为无信任时的恢复路径
- 新增 `jjn repo sync`（uv-sync 式收敛：clone/checkout 到 lock，静默 GC 孤儿
  lock 条目，不删目录）与 `jjn repo list`（name/target/rev/状态一屏）
- `repo add` 原子化：先物化后写 manifest；失败回滚本次创建的目录并声明
  "nothing written"；重名时打印已有条目全文 + remove 命令；`--target` 帮助
  文本写明是仓库本身路径
- 允许 clone 进已存在的空目录（对齐 `git clone`）；非空非 git 目录的拒绝
  信息包含语义解释与修复建议
- 错误信息纪律：事实 + 当前状态 + 一条具体修复命令
