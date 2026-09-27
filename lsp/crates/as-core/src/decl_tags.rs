//! `.d.as` 注解标签解析与 tag/doc 分流（架构设计 §2.4.3 / §2.4.4，D15）。
//!
//! 规则：
//! 1. **白名单驱动**：只有 §2.4.3 表中的 15 个 tag 是语义 tag（进 `DefData.tags`）；
//! 2. 其余一律视为 doc 正文原样保留（doxygen tag 由 hover 渲染层处理，不在本层）；
//! 3. 未登记 tag 一律忽略，**不报诊断**；
//! 4. 语义 tag 的 value 解析失败 → 丢弃该 tag，不报诊断（数据源是机器生成）。
//!
//! `@cache_format` 识别但**不消费**其值（D21）——`.d.as` 是源码非协议。
//! `@group` 的值与文件名末段同源，供 hover 显示完整包路径（§2.2.1 推论 2）。

/// 白名单 tag 种类（架构设计 §2.4.3 全表，15 项）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TagKind {
    /// 文件头，格式版本。识别但不消费（D21）
    CacheFormat,
    /// 文件头，分组 key（§2.2.1）
    Group,
    /// 属性仅 default 块内可赋值
    Editable,
    /// BlueprintCallable / BlueprintPure
    UFunction,
    /// 不可调用
    NotCallable,
    /// AS 名 ≠ UFunction 名时的原名
    UnrealName,
    /// 可 override 的 BlueprintEvent
    Event,
    /// 返回类型由第 N 实参决定（已扣除 hidden 参数）
    OutputTypeIndex,
    /// 非属性访问器（反向默认，§2.4.5）
    NotProperty,
    /// UFunction meta（键限于 Delegate* 四键；脚本函数为其全量 meta）
    Meta,
    /// asOBJ_TEMPLATE_SUBTYPE_COVARIANT
    TemplateCovariant,
    /// UE ScriptKeywords meta（`,`→`;` 分隔）
    Keywords,
    /// setter 仅 init-defaults 期有效
    DefaultsOnly,
    /// asOBJ_TEMPLATE_INHERIT_SPECIALIZATIONS
    TemplateInheritSpecializations,
    /// 该 class 块是模板特化的追加方法集（§4.3 引擎内部语法）
    TemplateSpecialization,
}

impl TagKind {
    /// tag 在源码里的拼写（解析键）。
    pub fn name(self) -> &'static str {
        match self {
            TagKind::CacheFormat => "cache_format",
            TagKind::Group => "group",
            TagKind::Editable => "editable",
            TagKind::UFunction => "ufunction",
            TagKind::NotCallable => "notCallable",
            TagKind::UnrealName => "unrealname",
            TagKind::Event => "event",
            TagKind::OutputTypeIndex => "outputTypeIndex",
            TagKind::NotProperty => "notProperty",
            TagKind::Meta => "meta",
            TagKind::TemplateCovariant => "template_covariant",
            TagKind::Keywords => "keywords",
            TagKind::DefaultsOnly => "defaultsOnly",
            TagKind::TemplateInheritSpecializations => "template_inherit_specializations",
            TagKind::TemplateSpecialization => "templateSpecialization",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "cache_format" => TagKind::CacheFormat,
            "group" => TagKind::Group,
            "editable" => TagKind::Editable,
            "ufunction" => TagKind::UFunction,
            "notCallable" => TagKind::NotCallable,
            "unrealname" => TagKind::UnrealName,
            "event" => TagKind::Event,
            "outputTypeIndex" => TagKind::OutputTypeIndex,
            "notProperty" => TagKind::NotProperty,
            "meta" => TagKind::Meta,
            "template_covariant" => TagKind::TemplateCovariant,
            "keywords" => TagKind::Keywords,
            "defaultsOnly" => TagKind::DefaultsOnly,
            "template_inherit_specializations" => TagKind::TemplateInheritSpecializations,
            "templateSpecialization" => TagKind::TemplateSpecialization,
            _ => return None,
        })
    }
}

/// tag 的值形态。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TagValue {
    /// 无值 flag
    Flag,
    /// 字符串值（Group / UnrealName；CacheFormat 解析失败的兜底也走这里）
    Text(String),
    /// 整数值
    Int(u32),
    /// `a;b;c` 列表（Keywords）
    Words(Vec<String>),
    /// `Key=Value` 对（Meta）
    Meta(Vec<(String, String)>),
}

/// 一个语义 tag。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SemanticTag {
    pub kind: TagKind,
    pub value: TagValue,
}

impl SemanticTag {
    fn flag(kind: TagKind) -> Self {
        SemanticTag { kind, value: TagValue::Flag }
    }
}

/// 分流产物：doc 正文 + 语义 tag 列表（均按出现顺序）。
#[derive(Clone, Debug, Default)]
pub struct DocBlock {
    /// doc 正文（未登记 tag 与普通注释行，原样逐行保留）
    pub doc: String,
    pub tags: Vec<SemanticTag>,
}

impl DocBlock {
    pub fn tag(&self, kind: TagKind) -> Option<&SemanticTag> {
        self.tags.iter().find(|t| t.kind == kind)
    }

    pub fn has_tag(&self, kind: TagKind) -> bool {
        self.tag(kind).is_some()
    }
}

/// 解析一段连续注释（传入各 comment 节点的原始文本，含 `//` 前缀）。
pub fn parse_comment_texts(texts: &[&str]) -> DocBlock {
    let mut doc_lines: Vec<String> = Vec::new();
    let mut tags: Vec<SemanticTag> = Vec::new();

    for raw in texts {
        let line = strip_comment_marker(raw);
        if let Some(rest) = line.strip_prefix('@') {
            // tag 名：字母/数字/下划线（`@TODO:` 之类的噪声自然落空）
            let name_end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let name = &rest[..name_end];
            let value = rest[name_end..].trim();
            if let Some(kind) = TagKind::from_name(name) {
                if let Some(tag) = parse_tag_value(kind, value) {
                    tags.push(tag);
                    continue;
                }
                // value 解析失败 → 丢弃该 tag，不报诊断（§2.4.4 规则 4）
                continue;
            }
            // 未登记 tag → doc 正文原样保留（含 '@'）
        }
        doc_lines.push(line.to_string());
    }

    DocBlock {
        doc: doc_lines.join("\n"),
        tags,
    }
}

fn parse_tag_value(kind: TagKind, value: &str) -> Option<SemanticTag> {
    match kind {
        TagKind::Editable
        | TagKind::UFunction
        | TagKind::NotCallable
        | TagKind::Event
        | TagKind::NotProperty
        | TagKind::TemplateCovariant
        | TagKind::DefaultsOnly
        | TagKind::TemplateInheritSpecializations
        | TagKind::TemplateSpecialization => Some(SemanticTag::flag(kind)),
        TagKind::Group | TagKind::UnrealName => {
            if value.is_empty() {
                None
            } else {
                Some(SemanticTag { kind, value: TagValue::Text(value.to_string()) })
            }
        }
        TagKind::CacheFormat => {
            // 识别但不消费（D21）；解析成功与否都不影响行为，失败时保留原文
            Some(match value.parse::<u32>() {
                Ok(n) => SemanticTag { kind, value: TagValue::Int(n) },
                Err(_) => SemanticTag { kind, value: TagValue::Text(value.to_string()) },
            })
        }
        TagKind::OutputTypeIndex => {
            value.parse::<u32>().ok().map(|n| SemanticTag { kind, value: TagValue::Int(n) })
        }
        TagKind::Keywords => {
            let words: Vec<String> =
                value.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
            if words.is_empty() {
                None
            } else {
                Some(SemanticTag { kind, value: TagValue::Words(words) })
            }
        }
        TagKind::Meta => {
            let (k, v) = value.split_once('=')?;
            let k = k.trim();
            let v = v.trim();
            if k.is_empty() || v.is_empty() {
                return None;
            }
            Some(SemanticTag { kind, value: TagValue::Meta(vec![(k.to_string(), v.to_string())]) })
        }
    }
}

/// 去掉 `//` 或 `/* */` 注释标记并去掉首尾空白。
fn strip_comment_marker(raw: &str) -> &str {
    let t = raw.trim();
    if let Some(rest) = t.strip_prefix("//") {
        return rest.trim();
    }
    if let Some(rest) = t.strip_prefix("/*") {
        let rest = rest.strip_suffix("*/").unwrap_or(rest);
        return rest.trim();
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &[&str]) -> DocBlock {
        parse_comment_texts(lines)
    }

    #[test]
    fn whitelist_tags_and_doc_split() {
        // 形态取自架构设计 §2.4.4 的实测样例
        let block = parse(&[
            "// Returns the location of the actor.",
            "// @ufunction",
            "// @unrealname K2_GetActorLocation",
            "// @see bGenerateOverlapEvents, ...",
            "// @param X the x",
        ]);
        assert_eq!(block.doc, "Returns the location of the actor.\n@see bGenerateOverlapEvents, ...\n@param X the x");
        assert_eq!(block.tags.len(), 2);
        assert_eq!(block.tags[0].kind, TagKind::UFunction);
        assert_eq!(block.tags[0].value, TagValue::Flag);
        assert_eq!(block.tags[1].kind, TagKind::UnrealName);
        assert_eq!(block.tags[1].value, TagValue::Text("K2_GetActorLocation".into()));
    }

    #[test]
    fn four_zero_corpus_tags_parse() {
        // 4 个「导出器已实现但语料零出现」的 tag（架构设计 §8 风险 8）——内置单测固化
        let block = parse(&[
            "// @keywords MoveTo;Teleport",
            "// @defaultsOnly",
            "// @template_inherit_specializations",
            "// @templateSpecialization",
        ]);
        assert_eq!(block.tags.len(), 4);
        assert_eq!(block.tags[0].kind, TagKind::Keywords);
        assert_eq!(block.tags[0].value, TagValue::Words(vec!["MoveTo".into(), "Teleport".into()]));
        assert_eq!(block.tags[1].kind, TagKind::DefaultsOnly);
        assert_eq!(block.tags[2].kind, TagKind::TemplateInheritSpecializations);
        assert_eq!(block.tags[3].kind, TagKind::TemplateSpecialization);
    }

    #[test]
    fn value_parse_failure_drops_tag() {
        // §2.4.4 规则 4：语义 tag 的 value 解析失败 → 丢弃，不报诊断
        let block = parse(&["// @outputTypeIndex abc"]);
        assert!(block.tags.is_empty());
        // 其余 value 形态失败同样丢弃
        let block = parse(&["// @meta novalue", "// @unrealname", "// @keywords"]);
        assert!(block.tags.is_empty());
    }

    #[test]
    fn unregistered_tags_stay_in_doc() {
        let block = parse(&["// @Indices", "// @TODO:", "// @return the thing", "// 普通注释"]);
        assert!(block.tags.is_empty());
        assert!(block.doc.contains("@Indices"));
        assert!(block.doc.contains("@TODO:"));
        assert!(block.doc.contains("@return the thing"));
    }

    #[test]
    fn meta_and_int_values() {
        let block = parse(&[
            "// @meta DelegateBindType=Dynamic",
            "// @outputTypeIndex 2",
            "// @cache_format 1",
            "// @group /Script/Engine",
        ]);
        assert_eq!(
            block.tag(TagKind::Meta).unwrap().value,
            TagValue::Meta(vec![("DelegateBindType".into(), "Dynamic".into())])
        );
        assert_eq!(block.tag(TagKind::OutputTypeIndex).unwrap().value, TagValue::Int(2));
        assert_eq!(block.tag(TagKind::CacheFormat).unwrap().value, TagValue::Int(1));
        assert_eq!(
            block.tag(TagKind::Group).unwrap().value,
            TagValue::Text("/Script/Engine".into())
        );
    }
}
