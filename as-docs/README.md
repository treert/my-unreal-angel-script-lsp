# as-docs — UE Angelscript 语言分析文档

> 本目录是 **Unreal Angelscript 语言的真值文档**：方言差异、类型语义、设计取舍的完整分析。
> 与 LSP 实现无关——无论用什么工具链实现，这里的结论都成立。
>
> 与 [`../docs/`](../docs/README.md) 的关系：**docs/ 单向引用 as-docs/**。as-docs 不引用
> 任何 LSP 设计（唯一的例外是各文档末尾「对本 LSP 的影响」章节，作为交付给 docs/ 的需求输入）。

## 文档索引

| 文档 | 一句话定位 | 关键内容 |
|---|---|---|
| [`原版AngelScript与UE-fork对比.md`](原版AngelScript与UE-fork对比.md) | **差异是什么**：fork 对原版 AS 改了什么 | §1 token 表增删（`@`/`null`/`is` 移除，`struct`/`n""`/f-string 新增）；§3 GC 被摘除的证据链；§5 paper features（parser 放行但永远不可用的构造，lambda/import 的终审）；§6 错误文本 → 诊断码的映射 |
| [`设计取舍与使用限制.md`](设计取舍与使用限制.md) | **差异为什么**：两条第一性原则推导全部限制 | 原则一「每个值必须回答生命周期归谁管」、原则二「每个构造必须能被反射描述」；§2 七项取舍因果链（GC/句柄/lambda/interface/容器/API 表面过滤/字段反射策略）；§3 脚本作者速查表「想要 X 用 Y」 |
| [`struct类型专题.md`](struct类型专题.md) | 值/引用二分的完整展开 | §4 class/struct 语义全量对照；§5 UE 映射（UClass vs UScriptStruct）；§6 继承四道锁与 UE 展平机制；§7 `FASStructOps` 行为由脚本方法驱动；§9 对本 LSP 的实现要求 |

## 推荐阅读顺序

1. `设计取舍与使用限制.md` §0-§1——先建立「引擎同构语言」的世界观和两条第一性原则；
2. `原版AngelScript与UE-fork对比.md`——按 token → 机制 → paper features 的顺序看具体差异；
3. `struct类型专题.md`——值/引用二分是类型系统（`AS01xx`/`AS02xx` 诊断多数码的语义背景）。

## 证据来源约定

三份文档统一使用只读参考路径（引擎源码为准，任何歧义以 VM 代码为最终真值）：

- `[ENGINE]` = `config.paths.unreal_engine`/Engine/Plugins/Angelscript/ThirdParty/source/
- `[UE]` = `config.paths.unreal_engine`/Engine/Plugins/Angelscript/Source/AngelscriptCode/

## 维护规则

- 新发现「砍了什么 / 替代品是什么 / 机制差异」→ 更新对应文档并追加其变更记录；
- 各文档自带版本号与变更记录表，修订必须留痕；
- 语言分析的结论**不得**依赖 Hazelight LSP 的行为（其实现有缺陷，如类型真值依赖引擎在线）。
