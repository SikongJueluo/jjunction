# 配置落盘位置与代码结构（决策记录）

- 日期：2026-09-23
- 状态：已接受
- 前置调研：`docs/research/rust-config-libraries.md`（选型：`jj_lib::config::StackedConfig` + `toml_edit`，TOML 为 canonical 格式）

## 背景

jjunction 有两层配置：全局（用户级）与项目本地（per-repo）。选型已定，本记录决定：
本地配置放在哪里、信任模型、以及 crate 内代码如何组织。

## 决策

### D1 项目本地配置 = 仓库根 `.jjunction/config.toml`（in-repo，默认不可信）

- 目录式命名，与 `.cargo/`、`.github/` 惯例一致；避免平文 `jj*.toml` 与 jj 自身
  配置名（历史 `~/.jjconfig.toml`）混淆；将来可在同目录下扩展
  （`templates/` 等）
- 随仓库分发：团队共享是本地配置的核心价值，外置模型做不到
- 与全局侧 `~/.config/jjunction/config.toml` 语义对称，两层同名文件
- 信任模型（对齐 git `safe.directory` 思路；jj 0.35 把 repo 配置移出仓库的动机同样适用于我们）：
  - 默认只接受**安全声明式子集**（白名单键；不含任何命令执行、外部路径影响）
  - 全量键的读取需在**全局配置中显式 trust 该仓库**（按仓库根路径记录）

### D2 优先级（低 → 高）

```
内置默认 (Default)
  → 仓库内 .jjunction/config.toml（untrusted，ConfigSource::User 先插入）
  → 用户全局 ~/.config/jjunction/config.toml（ConfigSource::User 后插入）
  → env 覆盖（将来）
  → CLI 覆盖（将来）
```

- 仓库内层**低于**用户全局：不可信内容不得覆盖用户显式选择（代价：团队共享键无法覆盖个人偏好，属预期行为）
- StackedConfig 实现映射：同 `ConfigSource` 的多层按插入序定优先级，后插入者高
  （jj v0.45 `insert_point` 源码已核实：`layers.len() - skip`，同 source 追加到既有层之后）
- phase 2（可选）：trusted 外置 per-repo 层（`ConfigSource::Repo`），供 `config edit --repo`
  做个人 per-repo 覆盖，与 jj 0.35+ 模型对齐

### D3 代码结构

- 单 crate，暂不拆 workspace；CLI 将来走 `src/bin/`
- `src/config/` 模块规划：

| 文件 | 职责 | 状态 |
| --- | --- | --- |
| `mod.rs` | `ConfigReader` trait、`JjunctionConfig`、文件名常量、层装配（`load_stacked`） | 已有 |
| `global.rs` | 用户全局层读取（`dirs` 解析平台配置目录） | 已有 |
| `local.rs` | 仓库内 untrusted 层读取 | 已有 |
| `trust.rs` | trust 门（`trusted-repos`）+ 安全键白名单 | 已有（白名单随后续键扩展） |

其余模块：

| 文件 | 职责 | 状态 |
| --- | --- | --- |
| `src/link.rs` | `[[link]]` 条目模型 + `apply` / `doctor`（首个 trust 门后的键） | 已有 |
| `src/workspace.rs` | jj-lib 多 workspace 枚举 + 主→其他文件 sync + direnv allow（内容闸） | 已有 |
| `src/bin/jjn.rs` | CLI：`jjn apply`（两阶段）/ `jjn doctor` | 已有（Linux/macOS；Windows 延后） |

## 后果与风险

- `ConfigSource::User` 承载两层是借用语义，升级 jj-lib 时需回归测试同源排序行为
- 将来支持 JSON/YAML 输入时以薄 adapter 转 `toml_edit::Value`，不引入第二套配置模型
- trust 记录存于全局层 → 全局配置文件格式需预留 `trusted-repos` 类键

## 变更响应（2026-09-23 补充）

采用 direnv 的 prompt 时检查模型，不引入 daemon：

- `.envrc` 中 `watch_file .jj/repo/workspace_store/index`（workspace 增删改写此文件）
  与 `watch_file .jjunction/config.toml`（配置变更）；`[ -d .jj/repo ]` 守卫避免
  secondary workspace（`.jj/repo` 为文件）watch 到错误路径
- watch 触发 direnv 重新求值 → enterShell 里的后台 `jjn apply --quiet` 重新同步
- 局限：反应粒度为下一次默认 workspace 的 prompt；纯 agent 非交互流程需 agent
  自行跑 `jjn apply`（写入 AGENTS.md 约定）
- 自动化（2026-09-23）：上述手工接线由 `jjn init` 幂等完成（.envrc 标记块 +
  devenv.local.nix 的 enterShell 钩子，见 `src/hooks.rs`）；doctor 会报告未接线状态
- 后手：`jjn watch` daemon（notify 监听 index/config/各 workspace 根，防抖后 apply），
  适用于纯 agent 建仓或链接自愈需求，暂不实施
