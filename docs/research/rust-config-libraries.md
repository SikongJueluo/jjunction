# Rust 配置解析库选型调研 — jjunction（基于 jj-lib 0.45 的 jj 工具集）

> 检索时间：2026-09（crates.io / docs.rs / GitHub 数据快照；最新可用版本以文中日期为准）
> 结论适用范围：`jjunction`（Rust 库，直接依赖 `jj-lib` 0.45.x）
> 标注约定：**直接证据** = 一手来源原文/源码；**解读** = 本调研基于证据的推断；**未验证** = 见第 6 节

---

## 0. TL;DR

1. **必须先修正任务前提**：jj 的配置栈**已经不是**基于 crates.io 的 `config` crate。jj-lib 从 **0.25.0（2025-01-01）** 起把配置底层数据结构迁移到 **`toml_edit`**（PR #5060，commit `4888641`），并把 `config` 从依赖中移除；jj-lib **0.45.1** 的依赖表里只有 `toml_edit ^0.25.12`，没有 `config`。因此"与 jj 生态一致"的正解是 **`toml_edit` + `jj_lib::config::StackedConfig`**，而不是 `config` crate。
2. **首选方案**：`jj_lib::config::StackedConfig`（`ConfigLayer`/`ConfigSource`）+ **`toml_edit` 0.25.x** 作为核心配置载体（TOML 作为 canonical 格式），外加一层很薄的 `FormatAdapter`（JSON/YAML → `toml_edit::Value`）来满足"格式未定"的要求。理由：零新增依赖重量（jj-lib 已经传递引入 toml_edit 0.25）、可无损写回（comments/formatting 保留）、错误信息带 dotted path + 源文件路径、可直接喂给 `UserSettings::from_config()` 复用 jj-lib 全部 reader。
3. **备选方案**：`config`（config-rs 0.15.26，2026-09-21 仍活跃发布）作为**多格式只读解析前端**，解析结果转换成 `toml_edit::Value` 后进入 StackedConfig；写回仍交给 `toml_edit`。代价：多一套 TOML/键名规范化语义、无写回、provenance 较弱。
4. **`figment`**：错误定位（provenance）最好、深合并语义最清晰，但 **0.10.19（2024-05-17）之后无正式发布**，且**没有写回 API**，published 版本还依赖已 deprecated 的 `serde_yaml 0.9` 与 `toml 0.8`；社区有 fork `figment2`。除非"错误信息质量"优先级压倒一切，否则只作次级备选。
5. **不建议**：`twelf`（末次发布 2024-03-11，依赖锁死 `toml ^0.5.8` / `serde_yaml ^0.8.23`，生态采用度极低）；`serde_yaml`（0.9.34+deprecated，官方 README 明示不再维护，YAML 需求应换 `yaml_serde`/`serde_norway` 等）。

---

## 1. 关键前提修正：jj 的配置栈到底长什么样（jj-lib 0.45 的事实）

| 事实 | 证据 |
| --- | --- |
| jj-lib **v0.24.0** 的 `lib/Cargo.toml` 含 `config = { workspace = true }` | 源码：<https://raw.githubusercontent.com/jj-vcs/jj/v0.24.0/lib/Cargo.toml>（直接证据，本次抓取） |
| jj-lib **v0.25.0 / v0.27.0 / v0.32.0 / v0.45.0** 的 `lib/Cargo.toml` **没有** `config`，只有 `toml_edit` | <https://raw.githubusercontent.com/jj-vcs/jj/v0.25.0/lib/Cargo.toml>、<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/Cargo.toml>（直接证据） |
| 迁移 PR：**"config: migrate underlying data structure to toml_edit" #5060**，2024-12-10 merged（3 commits，17 files） | <https://github.com/jj-vcs/jj/pull/5060>（直接证据；PR 内 `Cargo.toml +1/−1`） |
| 迁移 commit `4888641` 的 CHANGELOG 条目：`Configuration variables are no longer "stringly" typed. For example, true is not converted to a string "true", and vice versa.`，并在 `cli/src/config.rs` 删除 `to_toml_value()`（旧实现基于 `config::ValueKind`） | patch：<https://github.com/jj-vcs/jj/commit/4888641.patch>（直接证据） |
| 迁移进入 **jj 0.25.0（2025-01-01）**，release highlights 提到 "Improvements to configuration management" | <https://raw.githubusercontent.com/jj-vcs/jj/v0.25.0/CHANGELOG.md>（直接证据） |
| **jj-lib 0.45.1**（2026-09-03）依赖表：`toml_edit ^0.25.12`，无 `config` | <https://crates.io/crates/jj-lib/0.45.1>（直接证据） |

### 1.1 `jj_lib::config` 在 0.45.1 暴露了什么（互操作关键）

公开 API（docs.rs 0.45.1 模块页）：`StackedConfig`、`ConfigLayer`、`ConfigFile`、`ConfigSource`、`ConfigNamePathBuf`、`ConfigItem/ConfigTable/ConfigTableLike/ConfigValue`、`ConfigGetError/ConfigLoadError/ConfigUpdateError/ConfigMigrateLayerError`、`ConfigMigrationRule`、`ConfigResolutionContext`、`ToConfigNamePath`、`ConfigGetResultExt`，函数 `migrate()` / `resolve()`。
来源：<https://docs.rs/jj-lib/0.45.1/jj_lib/config/index.html>（直接证据）

- **`ConfigValue = toml_edit::Value`、`ConfigItem = toml_edit::Item`、`ConfigTable = toml_edit::Table`**：`jj_lib::config` 直接 re-export 的是**类型别名**，不是 toml_edit 本身。
  来源：<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/src/config.rs>（直接证据）
  → **解读**：jjunction 若要构造/编辑 `ConfigValue`，最省事的是自己加 `toml_edit = "0.25"`（与 jj-lib 同 semver 区间，不会出现两份 toml_edit）；简单值可用 `ConfigValue::from(42)` 之类的 `From` 实现（jj-lib 自己的测试即如此用：<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/src/settings.rs>）。
- **分层语义**：`StackedConfig` 文档明确写 "something like a read-only `overlayfs`… tables are merged across layers… There's no tombstone notation to remove items from the lower layers. Beware that arrays of tables are no different than inline arrays. They are values, so are never merged."（表深合并、值与数组整体覆盖、无删除标记）。合并实现 `merge_items()` 递归合并 table-like 节点。来源同上（直接证据）。
- **优先级顺序**：`ConfigSource` 枚举 = `Default < System < EnvBase < User < Repo < Workspace < EnvOverrides < CommandArg`，层按 `source` 排序保存。来源同上（直接证据）。
- **写回**：`ConfigFile::save()` 即 `fs::write(path, layer.data.to_string())`，写的是 toml_edit 的 format-preserving 文本；`ConfigFile::load_or_empty()` 对新文件自动写入 `#:schema https://docs.jj-vcs.dev/latest/config-schema.json` 头；`ConfigLayer::set_value()/delete_value()/ensure_table()` 提供带错误类型的编辑原语（`ConfigUpdateError::WouldOverwriteTable` 等）。来源同上（直接证据）。
- **直接对接 jj-lib 其它 API**：`UserSettings::from_config(config: StackedConfig) -> Result<Self, ConfigGetError>`、`UserSettings::config() -> &StackedConfig`、`get::<T>()/get_string()/get_bool()/get_int()/get_value()/get_table()/table_keys()`。来源：<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/src/settings.rs>（直接证据）
- **`jj_lib::config` 未 re-export `toml_edit`**：`lib/src/lib.rs` 只有 `pub mod config; mod config_resolver;`，未发现 `pub use toml_edit`。来源：<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/src/lib.rs>（直接证据）

### 1.2 jj 的"用户级 / 项目本地"两层在 jj 里怎么映射

- jj 文档明确列出四类来源：built-in defaults（不可编辑）、**user settings**（`jj config edit --user`）、**repo settings**（`--repo`，**出于安全原因不放在 repo 内**）、**workspace settings**（`--workspace`，同样不在 workspace 内），以及 command-line settings；前序被后序覆盖。
  来源：<https://docs.jj-vcs.dev/latest/config/>（直接证据）
- 用户配置文件加载顺序（后者覆盖前者）：`$HOME/.jjconfig.toml` → `<PLATFORM_SPECIFIC>/jj/config.toml`（推荐）→ `<PLATFORM_SPECIFIC>/jj/conf.d/*.toml`（字典序）。
  来源：同上（直接证据）
- 0.35.0 起 per-repo / per-workspace 配置**移出 repo**，`.jj/repo/config.toml`、`.jj/workspace-config.toml` 不再使用；CLI 侧有 `--repo/--workspace/--file` 与 `--config=NAME=VALUE`、`--config-file=PATH`。
  来源：<https://github.com/jj-vcs/jj/blob/v0.35.0/CHANGELOG.md>、<https://github.com/jj-vcs/jj/blob/v0.45.0/cli/src/config.rs>（直接证据）
- **解读（对 jjunction 的直接影响）**：如果 jjunction 的"项目本地配置文件"打算放在工作区里（例如仓库中的 `jjunction.toml` 或 `.jj/config.toml`），那与 jj 的威胁模型相反（仓库内容不可信）。jj-lib 为此还专门有 `pub mod secure_config`（同 `lib/src/lib.rs`）。建议要么沿用 jj 的"repo 配置存 repo 外"模型，要么对仓库内配置做显式的 trust/opt-in。

---

## 2. 候选逐一评估

### 2.1 `config`（config-rs）

| 维度 | 结论与证据 |
| --- | --- |
| 维护状态 | **活跃**。最新 **0.15.26 发布于 2026-09-21**（0.15.0 于 2024-12-17；0.15.1 于 2024-12-19），仓库 <https://github.com/rust-cli/config-rs>；总下载 1.15 亿、近 90 天 1855 万（crates.io API 快照）。来源：<https://crates.io/api/v1/crates/config>（直接证据） |
| 支持格式 | JSON / TOML / YAML / INI / RON / JSON5 / CORN，均可按 feature 裁剪；自定义格式通过 `Format` trait。来源：<https://raw.githubusercontent.com/rust-cli/config-rs/master/README.md>（直接证据） |
| 分层合并语义 | **深合并（deep merge）**：`src/path/mod.rs` 的 `Expression::set()` 对 `ValueKind::Table` 显式注释 `// Continue the deep merge` 并递归 `set`；非 table 值（含数组）整体覆盖。`Config::refresh()` 按 defaults → sources → overrides 重新构建缓存。来源：<https://docs.rs/config/latest/src/config/path/mod.rs.html>、<https://docs.rs/config/latest/src/config/config.rs.html>（直接证据） |
| env / CLI override | `Environment` source（prefix / separator / `convert_case` / `try_parsing` / `list_separator`）。**0.15.26 没有 `CommandLine` source**（docs.rs "List of all items" 只列 `Config`、`Environment`、`File`、`FileSourceFile`、`FileSourceString`、`Value` 与 builder 状态类型），CLI 覆盖需 app 侧用 `set_override()`/`set_default()` 注入。来源：<https://docs.rs/config/latest/config/all.html>、README（直接证据） |
| 错误信息质量 | **中等、有已知短板**。0.14.0 CHANGELOG 有 `[#413] Attach key to type error generated from Config::get_<type>()`；但 serde 反序列化错误会退化为 `ConfigError::Message`，丢失 key 与来源文件，见 issue **#532**（同一 issue 里也有人给出 `invalid digit found in string` 无 key 的复现）、issue **#371**（对比 figment 的 provenance，社区公认 figment 更好）；PR **#632**（引入 `serde_path_to_error` 做路径追踪）显示为 closed。来源：<https://github.com/rust-cli/config-rs/issues/532>、<https://github.com/rust-cli/config-rs/issues/371>、<https://github.com/rust-cli/config-rs/pull/632>（直接证据） |
| 写回/编辑 | **不支持**。README 原文：`Please note this library - can not be used to write changed configuration values back to the configuration file(s)!`，并说明 **key 会被小写化、大小写不敏感**。来源：README（直接证据） |
| 依赖重量 | 0.15.26 **必需 normal deps 仅 3 个**：`pathdiff`、`serde_core`、`winnow`；格式解析全部 optional（`toml ^1.0.6`、`serde_json`、`yaml-rust2 ^0.11`、`rust-ini`、`ron`、`json5`、`libcorn`、`convert_case`、`indexmap`、`serde-untagged`、`async-trait`）。注意 YAML 已从 serde_yaml 换成 `yaml-rust2`。来源：<https://crates.io/api/v1/crates/config/0.15.26/dependencies>（直接证据） |
| 与 jj-lib 互操作 | 无原生互操作。**解读**：需要把 `config::Value` 手工映射为 `toml_edit::Value` 再进 StackedConfig；两套 key 规范化规则（config-rs 小写化 vs TOML 原样）会造成语义摩擦 |

> 备注：README（master 分支）里的示例写的是 `config = "0.14.0"`，而 CHANGELOG（master）最新条目停在 `0.14.0 - 2024-02-01`（上方为 `## Unreleased`）。**解读**：0.15.x 的 changelog 未同步到 master（发布记录以 crates.io 为准），这不影响"仍在发版"的判断，但说明文档维护滞后。

### 2.2 `figment`

| 维度 | 结论与证据 |
| --- | --- |
| 维护状态 | **停滞（有 fork 风险）**。最新正式版 **0.10.19（2024-05-17）**，此后再无发布；仓库 <https://github.com/SergioBenitez/Figment>。issue **#148 "Maintenance status?"**（2025-10-15 开启，仍 open）中，作者 2026-04-18 回复"intend to continue maintaining… hope to have some time in the next few weeks"，社区同日/后续评论指出 **published 0.10.19 仍依赖 `toml = "0.8"`，而 master 早已切到 `toml_edit`（commit 6a363a1, 2024-05）但未发布**；社区 fork：**`figment2` 0.11.5**（crates.io，2025-12-03 首发，2026-04-20 最后更新），另有新 crate `compote` 0.3.0（2026-09-12，下载量仅 66，**过新不建议**）。来源：<https://github.com/SergioBenitez/Figment/issues/148>、<https://crates.io/crates/figment/versions>、<https://crates.io/crates/figment2>、<https://crates.io/crates/compote>（直接证据；"master 已迁移到 toml_edit" 一条为 issue 内用户评论，属第三方陈述） |
| 支持格式 | JSON / TOML / YAML / Env / `Serialized`（任意 serde 值，含 clap 解析结果）；带 feature gate。来源：<https://docs.rs/figment/latest/figment/>（直接证据） |
| 分层合并语义 | **语义最明确**：`join`/`adjoin`/`merge`/`admerge` 四种策略；对字典一律 "Union, Recurse"（深合并递归），数组默认按非复合值处理（`merge` 用新值、`admerge` 拼接），其它类型按策略整体覆盖。另有 `Profile`（default/global + 自定义 profile + `.nested()`）。来源：<https://docs.rs/figment/latest/figment/struct.Figment.html>、<https://github.com/SergioBenitez/Figment/blob/master/src/figment.rs>（直接证据） |
| env / CLI override | `Env::prefixed("APP_")` / `Env::raw()`；CLI 走 `Serialized::defaults(clap_matches)` 模式（官方文档给出与 clap 组合的推荐写法）。来源：<https://docs.rs/figment/latest/figment/>（直接证据） |
| 错误信息质量 | **最强项**：每个值都带 `Metadata` + `Profile` tag，跨 merge/join 保留，错误含 `path`；作者原话"perfectly track the provenance… emit error messages that point to the actual source"，另有 `RelativePathBuf` 这类基于 provenance 的 "magic" 值。来源：<https://docs.rs/figment/latest/figment/>、<https://github.com/rust-cli/config-rs/issues/371#issuecomment-…>（直接证据/当事方陈述） |
| 写回/编辑 | **不支持**。对照 `src/figment.rs` 全部 `pub fn`：`new/from/join/adjoin/merge/admerge/select/focus/extract/extract_lossy/extract_inner/extract_inner_lossy/metadata/profile/profiles/find_value/contains/find_metadata/get_metadata` —— **没有 `serialize`/`write`/`dump`**，也没有任何注释/格式保留能力。来源：<https://github.com/SergioBenitez/Figment/blob/master/src/figment.rs>（直接证据） |
| 依赖重量 | 0.10.19 必需 deps：`atomic`、`serde`、`uncased`（+ build-dep `version_check`）；optional：`toml 0.8`、**`serde_yaml 0.9`（已 deprecated）**、`serde_json`、`parking_lot`、`pear`、`tempfile`。来源：<https://crates.io/api/v1/crates/figment/0.10.19/dependencies>（直接证据） |
| 与 jj-lib 互操作 | 无。仍需把 `figment::Value` 映射到 `toml_edit::Value`；且 figment 的 profile 概念与 jj 的 `ConfigSource` 分层不是一回事（**解读**） |

### 2.3 `twelf`

| 维度 | 结论与证据 |
| --- | --- |
| 维护状态 | **事实停更**。最新 **0.15.0，发布于 2024-03-11**（检索时点已 2.5 年无发布）；总下载仅 35.2 万、近期 3.8 万（对比 config 的 1855 万近期下载）。来源：<https://crates.io/api/v1/crates/twelf>（直接证据） |
| 支持格式 | TOML / YAML / JSON / DHALL / INI（+ env / clap / 自定义闭包 / `Default` trait），通过 `Layer::*` 组合。来源：<https://github.com/bnjjj/twelf>、<https://docs.rs/crate/twelf/latest>（直接证据） |
| 分层合并语义 | README 原文："each layers override only existing fields"（**按字段覆盖**，层间不提供表级深合并开关）；实现方式是"每个 layer 产出一个 `serde_json::Value`，按层覆盖再反序列化"（**解读**：等价于浅层字段覆盖 + serde default 填充，缺少 jj/figment 那种表递归合并的显式语义） |
| env / CLI override | 有：`Layer::Env(Some("PREFIX_".into()))`（底层 `envy`，支持 HashMap/数组形式）、`Layer::Clap(...)`（proc macro 生成 `Conf::clap_args()`）。这是它的差异化卖点。来源：README（直接证据） |
| 错误信息质量 | 文档未说明 key/来源路径追踪；错误类型仅 `thiserror` 包一层（**未验证**，见第 6 节） |
| 写回/编辑 | 无（只有读取构建器），README 未提及任何写出能力（直接证据：README 全文无 write/serialize 描述） |
| 依赖重量 | 0.15.0 必需 deps：`config-derive`（proc macro）、`log`、`serde`、`serde_json`、`thiserror`；optional 里 **`toml ^0.5.8`、`serde_yaml ^0.8.23`、`serde_ini ^0.2`、`serde_dhall ^0.11`、`envy`、`shellexpand`、`dyn-clone`、`clap ^4`**；默认 features 含 `clap`。来源：<https://crates.io/api/v1/crates/twelf/0.15.0/dependencies>（直接证据） |
| 与 jj-lib 互操作 | 无；且旧 TOML/YAML 版本会与 jj-lib 的 `toml_edit 0.25` 并存（不同 crate，但拖入额外旧版本解析器） |

**结论：不推荐。** 停更 + 依赖版本锁死在 2019/2021 年代 + 采用度极低 + 强制 proc-macro/clap 耦合。

### 2.4 裸 serde + 各格式 crate

| crate | 维护/版本 | 读 | 写回/编辑 | 备注（来源） |
| --- | --- | --- | --- | --- |
| **`toml_edit`** | 0.25.15+spec-1.1.0（2026-09-11）；仓库 toml-rs/toml；总下载 8.34 亿 | ✅ | ✅ **format-preserving**：保留 comments/spaces/相对顺序；`DocumentMut` 可编辑根表，`Document::parse` 后 `into_mut()` | 文档明示限制：不保留 **dotted keys 顺序**、不保留 **缺失的行尾换行**。jj 的 `ConfigFile::save()` 就是这条路径。<https://crates.io/crates/toml_edit>、<https://docs.rs/crate/toml_edit/latest>、<https://docs.rs/toml_edit/latest/toml_edit/> |
| **`toml`** | 1.1.6+spec-1.1.0（2026-09-10）；总下载 9.28 亿 | ✅（serde 直反序列化，含 span 信息） | 序列化可用，但**不保留注释/格式**（"If you also need the ease of a more traditional API, see the toml crate"） | 适合"只读 + 强类型 DTO"；写回用户文件会破坏注释。<https://crates.io/crates/toml>、toml_edit 文档 |
| **`serde_json`** | 1.0.151；仓库 serde-rs/json；总下载 13.3 亿 | ✅ | ✅ 序列化（但不保留格式） | 事实标准 JSON；`Value` 可作中间模型。<https://crates.io/api/v1/crates/serde_json> |
| **`serde_yaml`** | **0.9.34+deprecated**（版本号自带 deprecated），最后更新 **2024-03-25**；README 原文 "(This project is no longer maintained.)" | ✅ | ✅（序列化，不保留注释） | **不要在新项目使用**。<https://crates.io/api/v1/crates/serde_yaml>、<https://github.com/dtolnay/serde-yaml> |
| YAML 替代：**`yaml_serde`** | 0.10.7（2026-08-18），仓库 <https://github.com/yaml/yaml-serde>（**YAML 官方组织维护**的 serde_yaml fork），crate 描述即 "serde_yaml maintained by The YAML Organization"；迁移方式为 Cargo `package =` 重命名，`use serde_yaml::` 可保持不变 | ✅ | ✅ | 目前最"正统"的 serde_yaml 接替者。<https://crates.io/api/v1/crates/yaml_serde>、<https://github.com/yaml/yaml-serde> |
| YAML 替代：**`serde_norway`** | 0.9.42（2024-12-21），仓库 cafkafk/serde-yaml，API 与 serde_yaml 0.9 同形；近期下载 429 万 | ✅ | ✅ | 社区硬 fork；无新发布已近 2 年。<https://crates.io/api/v1/crates/serde_norway> |
| YAML 其它 | `serde_yml` 据第三方迁移文档称 **2025-09 归档**并有 RUSTSEC-2025-0068（unsound+unmaintained）；`serde-saphyr`（无 `Value` DOM）、`serde-yaml-ng`、`noyalib` 等新选择并存 | — | — | **未验证**（第三方文档，见第 6 节）：<https://github.com/sebastienrousseau/noyalib/blob/main/MIGRATION.md> |

**解读**：裸 serde 组合的强项是"零魔法 + 极致可控"，但需要自己实现：多来源分层、来源路径错误信息、env/CLI override 优先级、以及"哪种格式"的运行时判定。若 jjunction 只做两层 TOML 且要写回，裸 `toml_edit` 反而是最贴合的实现方式（jj 已验证过一遍）。

### 2.5 jj-lib 自身（`StackedConfig` 作为"配置库"）

见第 1 节。优点：与 jj 语义完全一致（layer 顺序、表深合并、数组不合并、no tombstone）、自带 `ConfigFile::save()` 无损写回、`UserSettings::from_config` 可直接复用 jj-lib 的所有配置读取、错误类型自带 dotted name + `source_path`（`ConfigGetError::Type { name, error, source_path }`）、迁移/条件配置工具（`migrate()`、`resolve()`、`[scope]` 条件表）。
局限（**解读**）：① **TOML-only**（`ConfigLayer::parse` 就是 TOML 解析，`ConfigLoadError::Parse` 基于 `toml_edit::TomlError`），不满足"格式未定"字面要求；② jj-lib 是 pre-1.0，API 会随版本变动（0.45 已拆出 `jj-core` crate），且 `jj_lib::config` 未 re-export `toml_edit`，需自行对齐版本；③ 非 jj 场景下 `ConfigSource::{Repo, Workspace, EnvBase, EnvOverrides, CommandArg}` 需自行赋语义。

---

## 3. 维度对比矩阵

| 维度 | `config` 0.15.26 | `figment` 0.10.19 | `twelf` 0.15.0 | 裸 serde + `toml`/`toml_edit`/`serde_json` | `jj_lib::config` + `toml_edit` |
| --- | --- | --- | --- | --- | --- |
| 维护状态（2026-09） | ✅ 活跃（2026-09-21 发布） | ⚠️ 停更（2024-05 后无发布；作者称会继续，社区有 `figment2`） | ❌ 事实停更（2024-03） | ✅ `toml_edit`/`toml`/`serde_json` 均活跃 | ✅ 跟随 jj 主版本（0.45.1，2026-09-03） |
| 支持格式（"格式未定"） | ✅ JSON/TOML/YAML/INI/RON/JSON5/CORN | ✅ TOML/JSON/YAML/Env/Serialized | ⚠️ TOML/YAML/JSON/INI/DHALL（依赖老旧） | ✅ 任意（逐个加 crate）；**但需自建统一模型** | ❌ **TOML-only**（需自建 adapter） |
| 分层合并粒度 | ✅ 表深合并，值/数组覆盖 | ✅ 字典 Union+Recurse；数组可选拼接（`admerge`） | ⚠️ 字段级覆盖（无表深合并语义文档） | ⚠️ 自行实现 | ✅ 表深合并，值/数组覆盖，无 tombstone |
| env / CLI override | ✅ `Environment`（+自注入 override）；❌ 无 CLI source | ✅ `Env`；CLI 走 `Serialized::defaults(clap)` | ✅ `Layer::Env` + `Layer::Clap` | ⚠️ 自行实现 | ✅ `ConfigSource::{EnvBase,EnvOverrides,CommandArg}`；CLI 由调用方建层 |
| 错误信息质量（路径定位） | ⚠️ 类型错误带 key；serde 错误可能丢 key/来源（#532/#371） | ✅✅ provenance + path（最强） | ⚠️ 文档未说明 | ⚠️ 视格式 crate 而定（`toml` 有 span；`serde_json` 有 line/col，需自行拼路径） | ✅ `ConfigGetError::{NotFound{name},Type{name,error,source_path}}` |
| 写回/编辑（无损 round-trip） | ❌ README 明确不支持 | ❌ 无任何 write API | ❌ | ✅ **`toml_edit` 保留注释与格式**（dotted-key 顺序/行尾换行除外） | ✅ `ConfigFile::save()` + `set_value()/delete_value()`（底层即 toml_edit） |
| 依赖重量（direct，必需） | 3（`pathdiff`,`serde_core`,`winnow`）+ 可选格式 crate | 3（`atomic`,`serde`,`uncased`）+ 可选（含 deprecated `serde_yaml 0.9`/`toml 0.8`） | 5（`config-derive`,`log`,`serde`,`serde_json`,`thiserror`）+ 默认含 clap | 视组合：`toml_edit`（+`toml_datetime`/`toml_parser`/`winnow`…） | **新增 ≈ 0**（`toml_edit` 已由 jj-lib 传递引入；只需显式声明以使用其类型） |
| 与 jj-lib / jj 生态一致 | ❌（jj 已弃用） | ❌ | ❌ | ⚠️ 部分一致（toml_edit 同 jj） | ✅✅ `UserSettings::from_config(StackedConfig)` 直接复用 |
| 传输依赖数量（transitive） | **未测**（无法在本环境跑 `cargo tree`） | 未测 | 未测 | 未测 | 未测（但 jjunction 已依赖 jj-lib → toml_edit 已在图中） |

---

## 4. 推荐结论

### 4.1 首选：`jj_lib::config::StackedConfig` + `toml_edit`（TOML-first，薄 adapter 兜底"格式未定"）

**形态**：TOML 作为 canonical on-disk format；两层以上用 `ConfigSource::User`（全局）与 `ConfigSource::Repo`（项目本地）承载；写回走 `ConfigFile`；如需 JSON/YAML 输入，写一个 `FormatAdapter` 把外部值树转成 `toml_edit::Value`/`DocumentMut` 再入层。

```rust
use jj_lib::config::{ConfigFile, ConfigSource, StackedConfig, ConfigValue};
use jj_lib::settings::UserSettings;

// 读：global → project（后者覆盖前者，表级深合并）
let mut config = StackedConfig::with_defaults();          // jj-lib 内建默认（misc.toml）
config.load_file(ConfigSource::User, global_path)?;       // ~/.config/jjunction/config.toml
if project_path.exists() {
    config.load_file(ConfigSource::Repo, project_path)?;  // 项目本地
}
// 可选：env / CLI 高优先级层
let layer = /* ConfigLayer::with_data(ConfigSource::EnvOverrides, data) */;
config.add_layer(layer);

let value: bool = config.get("some.feature.enabled")?;    // 带 path 的错误信息
let settings = UserSettings::from_config(config.clone())?; // 复用 jj-lib 全部 reader

// 写：未来 `jjunction config set/edit`
let mut file = ConfigFile::load_or_empty(ConfigSource::User, global_path)?;
file.set_value("some.feature.enabled", true)?;            // 保留原文件注释/格式
file.save()?;
```

四个维度对照：
- **格式未定** → TOML 先落地（jj 生态已是 TOML，且 `#:schema`/Taplo 工具链成熟）；adapter 只做"外部格式 → toml_edit 值树"的转换，随时可替换格式而不动核心。**解读**：JSON 可无损映射到 TOML 数据模型（除 NaN/Inf 浮点）；YAML 映射有损（非字符串 key、anchor/alias、tag、重复 key），需在 adapter 里显式报错或降级。
- **要分层** → StackedConfig 原生支持 N 层 + 表深合并 + 数组整体覆盖 + 明确优先级枚举（`ConfigSource`），语义与 `jj` 完全一致，不会出现"jjunction 与 jj 合并结果不一致"的诡异 bug。
- **未来要写回** → `toml_edit` 是唯一给出"保留注释/空格/顺序"保证的选项（jj 的 `jj config set/unset/edit` 即建立其上）；`config`/`figment`/`twelf` 都不支持写回。
- **与 jj 生态一致** → 0 额外依赖重量（`toml_edit` 已在依赖图中，只需显式声明同 0.25.x）、`UserSettings::from_config()` 让 jjunction 的用户级配置可直接驱动 jj-lib；未来若 jj 调整 `StackedConfig` 语义，两者同步演进。

**落地注意**：
1. 需**显式添加 `toml_edit = "0.25"`**（jj-lib 未 re-export；`ConfigValue = toml_edit::Value`）。
2. jj-lib 是 pre-1.0（0.45 已拆出 `jj-core`），建议把 `jj_lib::config` 的使用约束在一个 `src/config/mod.rs` 内，便于随 jj 版本升级时集中改。
3. 项目本地配置若放在 repo 工作区内，请参考 jj 的威胁模型（repo 配置外置 / `jj_lib::secure_config`），或提供显式 trust 开关。

### 4.2 备选 A：`config`（config-rs）做"多格式只读前端" + `toml_edit` 写回

适用场景：产品明确要求在近期同时支持 TOML/JSON/YAML（甚至 INI/RON）用户配置，且愿意接受两套 key 规范化语义。
**理由**：0.15.26 仍在活跃发版（2026-09-21）、必需 deps 只有 3 个、格式支持最全、表深合并语义清晰、env source 成熟。
**代价/风险**：① 无写回（写回另配 `toml_edit`，会出现"两套配置模型"）；② key 一律小写化、大小写不敏感（README 明示）——与 TOML 原生语义冲突；③ serde 反序列化错误可能丢 key/来源（issue #532/#371）；④ 无 CLI source，CLI override 需 app 侧注入；⑤ 与 jj 生态零互操作，`UserSettings` 复用需要人工转换。

### 4.3 备选 B：`figment`（或 fork `figment2`）

适用场景：**错误信息/provenance 是第一优先级**，且近期不需要写回、能承担维护风险。
**理由**：provenance 保留 + 冲突策略四选一（`join/merge/adjoin/admerge`）+ profile 机制；作者 2026-04 表态继续维护。
**代价/风险**：0.10.19 自 2024-05 无发布；**无任何写回 API**（与"未来要写回"硬冲突）；published 依赖 deprecated 的 `serde_yaml 0.9` 与旧 `toml 0.8`；若采用 fork 则引入"非官方分支"的长尾风险（`figment2` 首版 2025-12；`compote` 2026-08 才发布、下载量两位数，不建议生产使用）。

### 4.4 不推荐

- **`twelf`**：停更 2.5 年、依赖锁死 `toml 0.5.8`/`serde_yaml 0.8.23`、采用度极低、无写回。
- **`serde_yaml`**：已 deprecated（版本号即 `0.9.34+deprecated`，README 声明不再维护）；YAML 需求请用 `yaml_serde`（YAML 官方组织 fork，0.10.7，2026-08）或 `serde_norway`（0.9.42）。
- **纯 `serde + toml`（无 toml_edit）**：可分用于只读 DTO，但一旦承担写回就会破坏用户文件中的注释与排版。

### 4.5 关于"格式未定"的工程判断（**解读**）

"格式未定"最容易被误读为"必须选一个多格式库"。实际上：
- 若最终只服务 jjunction 自身配置且与 jj 共存，**TOML 是事实上的既定选择**（jj 全线 TOML；jj-lib 的 `StackedConfig` 只吃 TOML）；
- 若"格式未定"指"未来可能读用户的 JSON/YAML 配置"，正确做法是**固定内部模型（`toml_edit::Value` 或自建 DTO）+ 可插拔 reader**，而不是让内部模型跟着库走。这既满足格式自由，又不牺牲写回与 jj 互操作。

---

## 5. 矛盾与分歧记录

1. **任务前提 vs 事实**：任务描述"jj 的 StackedConfig/ConfigLayer/ConfigSource 就是基于 crates.io 的 `config` crate 构建的 TOML 配置"。**该前提对 jj ≤0.24 成立，对 0.25+ 不成立**（见第 1 节证据）。这是本次调研最重要的纠正。
2. **figment 是否"不再维护"**：crates.io 上确实 2024-05 后无发布、master 有未发布改动，社区 issue #148 标题即 "Maintenance status?"；但作者 2026-04-18 明确表示仍在维护并计划发布新大版本。两种证据并存 → 结论应为"**发布节奏停滞、维护承诺存在但未兑现**"，而非"已归档"。
3. **`config` crate 的 master README/CHANGELOG 滞后于 crates.io**：README 仍写 `config = "0.14.0"`，CHANGELOG 最新条目停在 0.14.0，但 crates.io 显示 0.15.26（2026-09-21）。以 crates.io 为准（**解读**：0.15 线的 changelog 未回灌 master）。
4. **`serde_yml` 状态**：第三方迁移文档称其"2025-09 归档"且受 RUSTSEC-2025-0068 影响；本次未直接核对 RustSec DB 与仓库归档状态（见第 6 节）。
5. **jj 迁移 PR 与 Cargo.toml 变更的对应**：PR #5060 的文件列表显示 `Cargo.toml +1/−1`，但我核对过的单个 commit（`4888641`、`8080981`）patch 中未包含该行；依赖移除的**直接证据**是 v0.24.0 有 `config`、v0.25.0 起没有。

## 6. Missing evidence / 未验证项

1. **传递依赖数量（transitive dep count）未实测**：本环境只有读写与网络工具，无法运行 `cargo tree`/`cargo add`；第 3 节"依赖重量"列的是 crates.io 元数据里的 **direct dependencies**，不代表编译期 crate 总数。
2. **`twelf` 与 `figment` 的 commit 级活跃度未取到**：GitHub REST API 在本环境返回 `403 rate limit exceeded`；两者的维护性判断基于 crates.io 发布记录与 issue 线程（figment #148 覆盖到 2026-09-11 的评论）。
3. **`config` crate 0.15.26 的错误 key 追踪现状未逐行核对**：issue #532（缺 key/origin）、PR #632（`serde_path_to_error`，状态 closed）为历史证据；0.15.26 是否已通过其它 PR 改善，未验证。
4. **figment master → toml_edit 的迁移细节**仅来自 issue #148 的用户评论（commit `6a363a1`, 2024-05），未直接读该 commit。
5. **`serde_yml` 归档与 RUSTSEC-2025-0068**：来自第三方迁移文档，未核对 <https://rustsec.org> 与仓库归档状态。
6. **jj-lib 0.45 拆出 `jj-core` 后配置相关类型是否迁移**：仅确认 `jj-lib` 仍 `pub mod config`、依赖 `toml_edit`；未核对 `jj-core` 是否也承载配置类型。
7. **`toml` crate 1.x 与 `toml_edit` 0.25 的底层 parser 是否共享**：未核对内部依赖图（结论中只陈述"两个 crate 名同时出现在依赖树"，未断言解析器是否复用）。
8. **`jj_lib::config` 的 API 稳定性**：jj-lib pre-1.0，本次只验证 0.45.1 的公开面；跨版本兼容性未评估。

## 7. Sources

### Kept（关键来源）
- jj 配置源码（0.45.0）：<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/src/config.rs> — `StackedConfig`/`ConfigLayer`/`ConfigFile` 全部语义与写回实现（直接证据）
- jj settings 源码（0.45.0）：<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/src/settings.rs> — `UserSettings::from_config(StackedConfig)`（互操作关键）
- jj-lib 依赖表 0.24.0 / 0.25.0 / 0.45.0：<https://raw.githubusercontent.com/jj-vcs/jj/v0.24.0/lib/Cargo.toml>、<https://raw.githubusercontent.com/jj-vcs/jj/v0.25.0/lib/Cargo.toml>、<https://raw.githubusercontent.com/jj-vcs/jj/v0.45.0/lib/Cargo.toml> — `config` crate 移除时间点
- jj PR #5060：<https://github.com/jj-vcs/jj/pull/5060>；commit patch：<https://github.com/jj-vcs/jj/commit/4888641.patch>
- jj-lib 0.45.1 crates.io 页：<https://crates.io/crates/jj-lib/0.45.1>；`jj_lib::config` 文档：<https://docs.rs/jj-lib/0.45.1/jj_lib/config/index.html>
- jj 官方配置文档：<https://docs.jj-vcs.dev/latest/config/>；0.35.0 changelog（repo 配置外置）：<https://github.com/jj-vcs/jj/blob/v0.35.0/CHANGELOG.md>
- config-rs：crates.io <https://crates.io/api/v1/crates/config>、README <https://raw.githubusercontent.com/rust-cli/config-rs/master/README.md>、深合并源码 <https://docs.rs/config/latest/src/config/path/mod.rs.html>、条目列表 <https://docs.rs/config/latest/config/all.html>、依赖 <https://crates.io/api/v1/crates/config/0.15.26/dependencies>
- config-rs 错误质量：<https://github.com/rust-cli/config-rs/issues/532>、<https://github.com/rust-cli/config-rs/issues/371>、<https://github.com/rust-cli/config-rs/pull/632>
- figment：crates.io 版本 <https://crates.io/crates/figment/versions>、文档 <https://docs.rs/figment/latest/figment/>、冲突策略源码 <https://github.com/SergioBenitez/Figment/blob/master/src/figment.rs>、维护状态 issue <https://github.com/SergioBenitez/Figment/issues/148>、依赖 <https://crates.io/api/v1/crates/figment/0.10.19/dependencies>
- figment fork / 替代：<https://crates.io/crates/figment2>、<https://crates.io/crates/compote>
- twelf：crates.io <https://crates.io/api/v1/crates/twelf>、仓库 <https://github.com/bnjjj/twelf>、依赖 <https://crates.io/api/v1/crates/twelf/0.15.0/dependencies>
- 格式 crate：<https://crates.io/crates/toml_edit>、<https://docs.rs/crate/toml_edit/latest>（保留/不保留清单）、<https://crates.io/crates/toml>、<https://crates.io/api/v1/crates/serde_json>
- serde_yaml 现状与替代：<https://crates.io/api/v1/crates/serde_yaml>、<https://github.com/dtolnay/serde-yaml>、<https://crates.io/crates/yaml_serde>、<https://github.com/yaml/yaml-serde>、<https://crates.io/crates/serde_norway>

### Rejected / deprioritized
- Leapcell 博客《Flexible Configuration for Rust Applications…》— 二手、AI 味浓的概述，仅用于发现"figment 是否有写回"的问题，未作为结论依据
- `noyalib` 迁移文档（<https://github.com/sebastienrousseau/noyalib/blob/main/MIGRATION.md>）— 用于 YAML fork 全景，但为竞争 crate 的自述材料，只作线索并标注未验证
- `go-toml-edit`、`toml-edit-derive`、`tomledit`(PyPI) — 语言/用途不相关
- Debian/Guix 打包页 — 只旁证 figment 0.10.19 长期未更新，信息冗余
- `compote`（<https://crates.io/crates/compote>）— 2026-08 首发、66 次下载，缺乏生产验证，仅记录存在

## 8. Next steps（若继续推进）
1. 在 jjunction 仓库跑一次 `cargo tree -e normal` 实测 `config` / `figment` / `toml_edit` 的**传递依赖数量**，补齐第 3 节空缺（需允许执行 shell 的环境）。
2. 写一个 20 行 PoC：`StackedConfig::with_defaults()` + `ConfigSource::{User,Repo}` 两层 + `ConfigFile::set_value/save()`，验证注释保留与错误信息（`ConfigGetError::Type.source_path`）在真实文件上的表现。
3. 决定"项目本地配置"的落盘位置：沿用 jj 的 repo 外置模型，还是仓库内 + trust 开关（涉及 `jj_lib::secure_config`）。
4. 若坚持多格式：先定义内部模型边界（建议 `toml_edit::Value`），再评估 JSON/YAML adapter 的有损点清单（NaN/Inf、YAML 非字符串 key、anchor、重复 key）。
