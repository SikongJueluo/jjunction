# Git 操作栈选型调研 — gix in-process vs git 子进程

> 检索时间：2026-09-23（本地 cargo registry 一手源码 gix 0.87.1 / jj-lib 0.45.1 + GitHub PR/issue）
> 结论适用范围：jjunction sub-repo 管理（clone / fetch / rev 解析 / checkout / 状态读取）
> 标注约定：**直接证据** = 一手来源原文/源码；**解读** = 本调研基于证据的推断；**未验证** = 见第 7 节
> 源码速记：`REG = ~/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-4dc01642fd091eda`

---

## 0. TL;DR

1. **前提修正：jj-lib 0.45 不用 gix 做网络。** 它的 fetch/push 全部走 git 子进程，gix 只承担本地对象/ref/backend 职责。这是 jj 0.29→0.30 的既定路线（弃 libgit2，网络永久子进程化），不是临时妥协。
2. gix 0.87 若自走 HTTPS，必须启用 `blocking-http-transport-curl`（拉入 libcurl-sys C 栈）或 `blocking-http-transport-reqwest`（拉入 reqwest+hyper+tokio 整棵树，gitoxide 官方口径 experimental）。当前 jjunction 依赖树两者皆无 —— 任选都是净新增重量。
3. **推荐方案 A：v0 全部 git 子进程**（clone/fetch/checkout/状态读）。零新增依赖、与 west / jj 网络路径同构、天然吃满用户 gitconfig（代理 / insteadOf 镜像 / 凭据 —— 对 mihomo + 国内镜像环境这是正确性刚需，不是优化）。
4. **方案 B（后手，不进 v0）**：本地读用 gix（已在依赖树中，`src/head/` 的 HEAD 读取/peel 零 `cfg(feature)` 门槛，已可直接用），网络与 checkout 保持子进程 —— 即 jj 自己的分法。留给 doctor 高频路径按 profiling 结论再切。
5. **否决方案 C**：gix 全内进程含网络 —— 与 jj 主线方向相反、新增重量最大、绕开用户 git 网络配置，对本环境为负优化。

---

## 1. 事实：jj-lib 0.45 的 gix 使用现状

| 事实 | 证据 |
| --- | --- |
| jj-lib 0.45.1 依赖 `gix 0.87.1`，`default-features = false`，features 仅 `[attributes, blob-diff, index, max-performance-safe, sha1, sha256]`；**无任何网络 feature** | `$REG/jj-lib-0.45.1/Cargo.toml` L91-L104（直接证据） |
| 网络全部子进程：`Command::new(executable_path)` + `["fetch", "--porcelain", "--prune", "--no-write-fetch-head"]`（L183）、`["push", "--porcelain", "--no-verify"]`（L271）；`MINIMUM_GIT_VERSION = "2.41.0"`（`fetch --porcelain` 引入于 git 2.41，注释注明 2.40 仍在收安全补丁） | `$REG/jj-lib-0.45.1/src/git_subprocess.rs` L100/L183/L271/L44（直接证据） |
| gix 在 jj-lib 内的角色：git backend 的对象/ref 读写（`src/git.rs` 3615 行、`git_backend.rs`、`workspace.rs`）；remote 配置管理已移植到 gix（PR #5553） | 本地源码 grep `gix::` 命中分布；<https://github.com/jj-vcs/jj/pull/5553>（直接证据） |
| 演进史：PR #5228（"git: spawn a separate git process for network operations"）引入网络子进程 → 0.29 提供 `git.subprocess = true` 开关 → 0.30（PR #6048）移除开关，子进程成为**唯一**网络路径，libgit2 代码全删 | <https://github.com/jj-vcs/jj/pull/5228>、<https://github.com/jj-vcs/jj/pull/6048>、<https://github.com/jj-vcs/jj/releases/tag/v0.29.0>、CHANGELOG（直接证据） |
| 动机（官方口径）：SSH / 凭据 / 网络栈兼容性问题靠子进程一揽子解决（"full compatibility with Git's networking stack, credential helpers, and server-side protocols"） | <https://github.com/jj-vcs/jj/issues/4979>（"Resolve many SSH issues by having networked `jj git` commands shell out to `git`"）、tracking issue #5548（git2 deprecation）（直接证据） |

**解读**：jj 从 git2 → gix 的迁移只覆盖**本地**职责；网络面经历了"库内实现（git2/gix）→ 可选子进程 → 强制子进程"的单向演进，社区无回退迹象。我们直接站在终态上即可。

## 2. gix 0.87 网络能力评估

- clone/fetch API 存在（`gix::clone::PrepareFetch` → `PrepareCheckout`，`$REG/gix-0.87.1/src/clone/`），但真正发起 fetch 被 `blocking-network-client` / `async-network-client` feature 门控（`src/clone/mod.rs` 大量 `cfg(feature)`，直接证据）。
- HTTPS 传输后端二选一（`Cargo.toml.orig` features 表，直接证据）：
  - `blocking-http-transport-curl` → `gix-transport/http-client-curl` → **libcurl-sys（C 编译栈）**。gitoxide 口径：curl 是 production-ready（与 git 本体同栈）。
  - `blocking-http-transport-reqwest(-rust-tls / native-tls)` → **reqwest + hyper + tokio 整棵树**。gitoxide 口径：experimental，部分共享 HTTP 选项不支持。
  - 另有文档-feature 名不一致的历史问题佐证其成熟度：<https://github.com/GitoxideLabs/gitoxide/issues/2425>
- 本仓库 `Cargo.lock` 中 **无 reqwest / libcurl-sys / curl-sys / tokio / hyper 任何条目**（直接证据，本次 grep）→ 任何 HTTP 后端都是净新增重量，违背"零新增重量"选型准则。
- SSH 传输：gix 同样 spawn 系统 `ssh`（子进程），无内进程优势（**解读**，基于 gix-transport ssh 走 command 的公开设计；未逐行验证）。

## 3. 操作清单 → 各方案映射

| 需求 | A 全子进程 | B gix 本地读 + 子进程网络 | C gix 全内进程 |
| --- | --- | --- | --- |
| clone | `git clone --no-checkout <url> <dir>` | 同 A | `PrepareFetch` + checkout，需新增网络 features |
| fetch | `git -C <dir> fetch --no-tags origin <rev>` | 同 A | 需 curl/reqwest 栈 |
| rev→SHA | fetch 后 `git rev-parse --verify <rev>^{commit}`（tag 自动 peel，免解析 ls-remote wire format） | 同 A | 需 `revision` feature（jj-lib 未启用） |
| checkout | `git -C <dir> checkout --detach <sha>`（脏树自动拒绝，即天然安全闸） | 同 A；或 gix `worktree-mutation`（新增 gix-worktree-state 等，API 风险面最大） | 同左 |
| 读 HEAD SHA | `git -C <dir> rev-parse HEAD` | `repo.head()` 系（`src/head/` 零 `cfg(feature)`，已可用） | 同左 |
| 脏检测 | `git -C <dir> status --porcelain` | 需启用 gix `status` feature（gix-status/gix-dir，纯 Rust 小件） | 同左 |

**解读**：A 的每一项都是十年稳定的 CLI 面，且 checkout 的脏树保护是 git 自带行为；B 的增量收益（省几次进程 spawn）只在 doctor 高频轮询场景有意义；C 三个维度全面劣化。

## 4. 决策建议

> **勘误（2026-09-23，同日）**：本节原推荐"v0 = A 全子进程"。后续讨论（sub-repo 定为 readonly、用户偏好 in-process 读）后实际决策为混合形态：**gix in-process 管"看"（本地读，零 feature 门槛）+ git 子进程管"动"（clone/fetch/checkout）**。事实部分（第 1–3 节）不变，终版见 `docs/design/subrepo.md` D5。

- **v0 = A。** 理由：零新增依赖（延续配置选型第一准则）；与生态同构 —— jj 0.30+ 网络面本就要求 PATH 有 git，对本项目用户不构成额外前提；用户 gitconfig 的代理/镜像/凭据全数生效（mihomo tun + 镜像环境下，`insteadOf` 重写与 http.proxy 是否生效决定功能可用性）。
- doctor/apply 的 repo 状态读取 v0 也走子进程；若将来 profiling 显示 spawn 开销可观，再局部切 B（gix 本地读已零成本在场，切换是局部改动，不破坏接口）。
- 版本下限：v0 不用 `fetch --porcelain`（只看退出码 + 事后 `rev-parse`），实际下限为任何支持 `rev-parse <rev>^{commit}` 的 git；将来若需解析 fetch 输出，对齐 jj 的 2.41。

## 5. 实现备忘（与选型正交）

1. **rev 解析走 rev-parse 而非 ls-remote**：fetch 后本地 `git rev-parse --verify <rev>^{commit}`，branch/tag/SHA 三态统一（tag peel 到 commit），少依赖一个 wire format。
2. **浮动 rev（缺省 = 跟随默认分支）**：本地解析 `refs/remotes/origin/HEAD`（clone 时 git 写入）；缺失时 fallback `git ls-remote --symref <url> HEAD`。远端改默认分支导致的陈旧由 doctor 提示。
3. **SHA rev 的 fetch 限制**：按 SHA fetch 依赖服务端 `uploadpack.allow*SHA1InWant`（GitHub 支持任意 SHA；自建服务端不一定）。策略：本地已有 → 直接用；否则 `fetch origin <sha>`，失败退全量 `+refs/heads/*:refs/remotes/origin/*` 再 rev-parse，仍无则报错并解释原因。
4. **clone 用 `--no-checkout`**：避免先物化默认分支再 checkout 目标 SHA 的双份工作树写入。
5. **脏树安全**：`checkout --detach` 遇工作树冲突性改动时非零退出，作为 apply 跳过该仓的信号；doctor 用 `status --porcelain` 呈现。
6. 后手优化（不进 v0）：partial clone（`--filter=blob:none`）、全局 mirror + `--reference`（对应 west/repo tool 的 mirror 概念；须遵守"cache 非 correctness 依赖"原则——删缓存后 sync 仍可完整重建）。

## 6. 与既有架构约束的对齐

- "jj-lib 是唯一 jj 交互通道"约束的是 **jj**，不含 git；git 子进程不违反（且 `src/workspace.rs` 已有 direnv 子进程先例）。
- trust 门：`[[repo]]` 与 `[[link]]` 同门。子进程面 == 用户手动跑 git 的面（clone 不执行远端代码；checkout 触发的钩子仅来自用户本地 hooksPath 配置），无额外提权面。

## 7. 未验证

- gix ssh transport 是否确为 spawn 系统 ssh（第 2 节标注处）
- `git fetch origin <sha>` 在 GitHub 之外托管（GitLab/Gitea/自建）的 allowAnySHA1InWant 支持矩阵
- `refs/remotes/origin/HEAD` 在 `--no-checkout` clone 下是否始终写入（git ≥2.8 应当写入，未实测）
- Windows 路径（明确延后，不适用）
