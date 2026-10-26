# 改动记录 → Release 说明

发版说明（Release body）不是自动生成的。本仓库不走 Pull Request，GitHub 的
`generate_release_notes` 只会产出一行

```
**Full Changelog**: https://github.com/ntc0728/agentkeeper/compare/v0.2.0...v0.2.1
```

而**应用内的更新面板显示的就是这段文字**。所以说明必须人手写，并且按人读得懂的
格式写在仓库里。

---

## 1. 记录在哪

| 文件 | 作用 |
|---|---|
| `changelog/next.json` | **一个发布周期一份**。改代码时往里写，发版时 CI 把它渲染成 Release 说明，发完清空 |
| `scripts/changelog.js` | 增 / 查 / 校验 / 渲染 / 清空，五个动作 |

```bash
npm run changelog:show                        # 看记录 + 它会渲染成什么样
npm run changelog:add -- fix "…"              # 追加一条（kind: fix / feat / opt）
npm run changelog:summary -- "一句话标题"      # 这次发版的标题行
npm run changelog:check                       # 校验（CI 与本地同一套规则）
npm run changelog:reset                       # 发完版清空
```

直接编辑 `changelog/next.json` 等价：

```json
{
  "summary": "修复已安装的 0.2.0 永远检测不到更新",
  "entries": [
    { "kind": "fix", "text": "发布构建与上一版同为 0.2.0（版本号未递增），软件内检查更新恒定判为「已是最新」" }
  ]
}
```

`kind` 只有三个：`fix` → 修复、`feat` → 新增、`opt` → 优化。**编号、以及
「修复 / 新增 / 优化」前缀都由渲染器生成**，记录里不要自己写 `1.`，否则编号会重复。

## 2. 记录什么

只记**用这个构建的人能感知到的代码改动**：

- 行为、缺陷修复、性能、可感知的交互与文案调整；
- 会影响已有数据的迁移、兼容性变化。

不要记：

- 纯文档、注释、格式化；
- CI / 构建脚本 / 依赖升级这类用户看不见的改动（除非它改变了产物的行为）；
- 重构——**可观察行为没变的重构不算改动**，写进去只会让说明变长变糊。

一次改动对应一条；同一件事在一次发布里只写一条，写完还在改就直接改那一条，不要追加第二条。

## 3. 格式（渲染结果）

```
0.2.1: 修复已安装的 0.2.0 永远检测不到更新

1. 修复: 发布构建与上一版同为 0.2.0（版本号未递增），软件内检查更新恒定判为「已是最新」
2. 优化: 更新检查的失败原因不再被当成「没有发布版本」静默吞掉
```

- 第一行 `版本: 标题` —— 版本取自 tag（`v0.2.1` → `0.2.1`），不写在文件里；
- 每条一行，短句，说清「哪坏了 / 现在怎么」；
- 只有列出来的是人看的东西，`changelog/next.json` 里不存版本号。

## 4. 发版流程

1. 改代码，同时更新 `changelog/next.json`（`npm run changelog:add -- …`）；
2. **先提交**（记录要和代码在同一个 commit 里，CI 才看得到）；
3. 打 tag 并推送：`git tag v0.2.2 && git push origin main v0.2.2`；
4. CI 的 tag 构建会跑 `node scripts/changelog.js render --version <tag>`，
   把结果作为 `body_path` 发布；**记录为空或格式不对，这一步直接失败**，
   不会发出一个没人看得懂的版本；
5. 发完执行 `npm run changelog:reset`（并提交），下一个版本重新写。

main 分支的推送仍然用 GitHub 自动生成的说明——那只是 CI 构建，不是给人读的发布。

## 5. 为什么不累加历史

一个 Release 的说明只描述这一个版本。文件里累积历史，等于每次发版都要在旧内容里
挑出本次的部分，迟早会改错已发布的版本；而历史本来就存在——每个发出去的说明都
留在 Releases 页面。所以 `next.json` 是**覆盖式**的：发完清空，新记录直接替换旧记录。

## 6. 校验

```bash
npm run changelog:check     # 结构、kind 取值、空文本、重复条目、自编号、空标题
npm run changelog:show      # 预览渲染结果
```

CI 里 `render` 走的是同一份校验，所以本地过 = 发版不会因为格式失败。

## 7. 提交信息同一套写法

提交标题和改动记录说同一句话，避免「commit 写英语、changelog 写中文、发版再翻译一遍」：

```
修复: 设置页「最新版本」读不到发布版本时显示「未知」
新增: 更新来源改为 GitHub 图标，点击打开仓库
优化: 更新日志改为标题在上、说明在下
```

- 三种前缀与 `next.json` 的 `kind` 一一对应：`fix` → 修复、`feat` → 新增、`opt` → 优化；
- 标题就是那条改动记录的文字（可以更短，但不要换个说法）；
- 钩子由仓库提供，装一次即可：

```bash
npm run hooks:install     # git config core.hooksPath .githooks
```

`.githooks/commit-msg` 会拒绝不符合上述前缀的标题（`Merge` / `Revert` 除外）。
纯 CI、纯文档、纯格式改动不进 `next.json`，但提交标题仍用同一套前缀。
