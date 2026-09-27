//! 说明符 schema 静态表（M5c，规划 §8.1 M5「说明符补全 schema 驱动」）。
//!
//! **取证来源（表内容真值）**：引擎 `AngelscriptPreprocessor.cpp` 的
//! `PP_NAME_*` 常量消费区间——按 `Spec.Name == PP_NAME_X` 的行号落进哪个
//! 解析函数归类（UCLASS = ProcessClassMacro [2106,2237)、
//! UFUNCTION = ProcessFunctionMacro [1232,1795)、
//! UPROPERTY = ProcessPropertyMacro [2237,2700)、UENUM = DetectEnum
//! [2700,2810)）。**doc 文本**取自 Hazelight `specifiers.ts`（MIT，行为
//! 参考）——引擎不提供说明符的描述文案，官方 LSP 的描述是唯一现成来源。
//!
//! 取证结论与官方表的三处差异（引擎为准）：
//! - 官方缺 `EditorOnly` / `Exec` / `WithValidation` / `ForcedAssets` /
//!   `ClassGroupNames`（引擎消费，官方表无）；
//! - 官方多的 `AutoCollapseCategories` 引擎只声明 `PP_NAME_` 未消费——
//!   不收录（「语法接受 ≠ 语义合法」同族：补全只给引擎真消费的）；
//! - 官方 `ASStructSpecifiers` 有 Meta，引擎 USTRUCT 解析无 `Spec.Name`
//!   消费（AnalyzeStructs 只处理继承禁止 + 文件级 EditorOnly meta）——
//!   USTRUCT 表为空，grammar 的 `ustruct_specifiers` 语境也不给候选。
//!
//! 带值（`Name=value`）判定 = 引擎分派读取 `Spec.Value` 的（AttachSocket /
//! BlueprintGetter / BlueprintSetter / ClassGroup / Config / DefaultConfig /
//! DisplayName / EditFixedSize / ReplicatedUsing / ReplicationCondition /
//! RootComponent / ToolTip）∪ 常识形态（Meta / Category / Keywords /
//! HideCategories / ClassGroupNames / Attach——子列表或字符串实参）。
//!
//! `Meta=(...)` 的**子说明符值补全**（EditCondition / ClampMin 等）不在
//! 本期——官方表有 4 组子表，消费面窄，按需再取证。

/// 宏 → 其说明符表。
pub fn specifiers_of(macro_name: &str) -> &'static [SpecifierInfo] {
    match macro_name {
        "UCLASS" => UCLASS,
        "UFUNCTION" => UFUNCTION,
        "UPROPERTY" => UPROPERTY,
        "UENUM" => UENUM,
        _ => &[],
    }
}

/// 说明符语境节点 kind → 宏名（as-syntax node.rs 的 6 个 *_specifiers；
/// `ustruct_specifiers` 引擎无消费 → None → 不给候选）。
pub fn macro_of_specifier_node(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "uclass_specifiers" => "UCLASS",
        "ufunction_specifiers" => "UFUNCTION",
        "uproperty_specifiers" => "UPROPERTY",
        "uenum_specifiers" => "UENUM",
        _ => return None,
    })
}

/// 一个说明符项。
pub struct SpecifierInfo {
    pub name: &'static str,
    /// `Name=value` 形态（insert 补 `=`）
    pub takes_value: bool,
    /// 描述（Hazelight specifiers.ts，MIT）
    pub doc: &'static str,
}

impl SpecifierInfo {
    pub fn insert(&self) -> String {
        if self.takes_value {
            format!("{}=", self.name)
        } else {
            self.name.to_string()
        }
    }
}

macro_rules! spec {
    ($name:literal, $v:literal, $doc:literal) => {
        SpecifierInfo { name: $name, takes_value: $v, doc: $doc }
    };
}

static UCLASS: &[SpecifierInfo] = &[
    spec!("Meta", true, "Specify arbitrary meta tags"),
    spec!("HideCategories", true, "Properties in these categories are not editable on this class"),
    spec!("ComponentWrapperClass", false, "Actor is a lightweight wrapper around a single component"),
    spec!("Placeable", false, "Class can be placed in the level or on an actor by the editor"),
    spec!("NotPlaceable", false, "Class cannot be placed in the level or on an actor by the editor"),
    spec!("NotBlueprintable", false, "Blueprints cannot be choose this as a parent class"),
    spec!("Blueprintable", false, "Blueprints can be created with this as a parent class"),
    spec!("Abstract", false, "Cannot be instantiated on its own, must have a child class to spawn"),
    spec!("Transient", false, "All instances of this class will be transient"),
    spec!("HideDropdown", false, "This class will be hidden from property combo boxes in Editor"),
    spec!("Deprecated", false, "This class is deprecated and should not be used"),
    spec!("Config", true, "Allow properties in this class to be saved and loaded to the specified ini"),
    spec!("DefaultConfig", true, "Config properties on this class should be saved to default configs, not user configs"),
    spec!("ClassGroup", true, "List this class under the specified group in the editor"),
    spec!("ClassGroupNames", true, "List this class under the specified group names in the editor"),
    spec!("DefaultToInstanced", false, "Indicates that references to this class default to instanced"),
    spec!("EditInlineNew", false, "Class can be constructed from editinline New button"),
];

static UFUNCTION: &[SpecifierInfo] = &[
    spec!("ToolTip", true, "Tooltip to show in the editor"),
    spec!("EditorOnly", false, "Function is only available in editor builds"),
    spec!("BlueprintCallable", false, "Function can be called from blueprint"),
    spec!("NotBlueprintCallable", false, "Function is not available in blueprint at all"),
    spec!("BlueprintPure", false, "Function is a pure node in blueprint without an exec pin"),
    spec!("BlueprintEvent", false, "Function can be overridden by child blueprint classes"),
    spec!("NetFunction", false, "Function is a NetFunction"),
    spec!("CrumbFunction", false, "Function is a CrumbFunction"),
    spec!("NetMulticast", false, "The function is executed both locally on the server, and replicated to all clients, regardless of the Actor's NetOwner"),
    spec!("WithValidation", false, "Network function requires a validation implementation"),
    spec!("Client", false, "The function is only executed on the only client if called from the server"),
    spec!("Server", false, "The function is only executed on the server if called from the owning client"),
    spec!("Unreliable", false, "Network function is sent as unreliable"),
    spec!("BlueprintOverride", false, "Override a BlueprintEvent in a parent script or C++ class"),
    spec!("Meta", true, "Specify arbitrary meta tags"),
    spec!("DisplayName", true, "Name to use to display the function in the editor"),
    spec!("Keywords", true, "Keywords this can be found by in the editor"),
    spec!("Category", true, "Category to list this under in the editor"),
    spec!("DevFunction", false, "Function is a DevFunction"),
    spec!("CallInEditor", false, "Create a button in the details panel to call this function in the editor"),
    spec!("ForcedAssets", false, "Forces this function to be included even if unreferenced"),
    spec!("BlueprintAuthorityOnly", false, "This function will only execute from Blueprint code if running on a machine with network authority (a server, dedicated server, or single-player game)"),
    spec!("Exec", false, "Function can be executed from the editor console"),
    spec!("BlueprintProtected", false, "Treat this function as protected in blueprint, disallowing it to be called by non-child blueprints"),
];

static UPROPERTY: &[SpecifierInfo] = &[
    spec!("ToolTip", true, "Tooltip to show in the editor"),
    spec!("EditorOnly", false, "Property is only available in editor builds"),
    spec!("Meta", true, "Specify arbitrary meta tags"),
    spec!("DisplayName", true, "Name to use to display in the editor"),
    spec!("Keywords", true, "Keywords this can be found by in the editor"),
    spec!("Category", true, "Category to list this under in the editor"),
    spec!("BlueprintProtected", false, "Treat this property as protected in blueprint, disallowing it to be edited by non-child blueprints"),
    spec!("BlueprintSetter", true, "Specify a function to call instead when writing this property from blueprint"),
    spec!("BlueprintGetter", true, "Specify a function to call instead when reading this property from blueprint"),
    spec!("Transient", false, "Property is never saved into the on-disk asset"),
    spec!("Config", true, "Property can be saved and loaded from config ini files"),
    spec!("Interp", false, "Property can be modified by sequence tracks"),
    spec!("AssetRegistrySearchable", false, "Property is indexed for searching in the Asset Registry"),
    spec!("NoClear", false, "Property is not allowed to be changed to nullptr"),
    spec!("DefaultComponent", false, "Component will be created as a default component on the actor"),
    spec!("OverrideComponent", true, "Specify a component in the parent class to override the class type of"),
    spec!("BindComponent", false, "Automatically bind this property to the component with the same name within child actor blueprints"),
    spec!("ShowOnActor", false, "Use on DefaultComponents, properties from the component will appear in the actor's details panel"),
    spec!("RootComponent", true, "Use on DefaultComponents, specify that this component should be the root component of the actor"),
    spec!("Attach", true, "Use on DefaultComponents, specify a different component to attach this to in the scene hierarchy"),
    spec!("AttachSocket", true, "Use on DefaultComponents with an Attach, specify a socket to attach to on this component's attach parent"),
    spec!("BlueprintReadWrite", false, "Allow the property to be read and written from blueprint nodes"),
    spec!("BlueprintReadOnly", false, "Allow the property to be read from blueprint but not written"),
    spec!("BlueprintHidden", false, "Do not make this property available to blueprint at all"),
    spec!("NotVisible", false, "Property cannot be changed or seen in the details panel at all"),
    spec!("NotEditable", false, "Property cannot be edited from unreal anywhere"),
    spec!("EditConst", false, "Property can be seen in the details panel but not edited"),
    spec!("EditInline", false, "Edit the values of this object inline in its container"),
    spec!("EditInlineDefaults", false, "Edit the values of this object inline in defaults only"),
    spec!("EditInstanceOnly", false, "Property can only be changed on instances in the level"),
    spec!("EditDefaultsOnly", false, "Property can only be changed on defaults inside blueprint classes"),
    spec!("VisibleInstanceOnly", false, "Property can only be seen on instances in the level, but not changed"),
    spec!("VisibleDefaultsOnly", false, "Property can only be seen on defaults inside blueprint classes, but not changed"),
    spec!("VisibleAnywhere", false, "Property can be seen both on blueprint classes and instances in the level, but not changed"),
    spec!("EditAnywhere", false, "Property can be changed by blueprint classes and on instances in the level"),
    spec!("Replicated", false, "Property should be replicated to clients"),
    spec!("ReplicationCondition", true, "Specify when the property should be replicated"),
    spec!("ReplicationPushModel", false, "Use Push Model to replicate this property"),
    spec!("ReplicatedUsing", true, "Specify a function to call when the property is replicated (requires Replicated)"),
    spec!("EditFixedSize", true, "Use on TArray properties, the size of the array cannot be changed from the editor"),
    spec!("NotReplicated", false, "Property is not replicated even if the struct it is in is replicated"),
    spec!("Instanced", false, "The object in this property is a new instance for each containing instance"),
    spec!("SkipSerialization", false, "Property is never serialized"),
    spec!("SaveGame", false, "Property should be serialized for save games"),
    spec!("AdvancedDisplay", false, "Property can only be edited after expanding to advanced view"),
    spec!("ExposeOnSpawn", false, "Property should be available to be changed when spawning this object from blueprint"),
    spec!("BindWidget", false, "Automatically bind this property to the widget with the same name within child UMG blueprints"),
    spec!("BindWidgetAnim", false, "Automatically bind this property to the widget animation with the same name within child UMG blueprints"),
    spec!("BindWidgetOptional", false, "Optionally bind this property to the widget with the same name within child UMG blueprints (no error if widget is missing)"),
];

static UENUM: &[SpecifierInfo] = &[
    spec!("Meta", true, "Specify arbitrary meta tags"),
    spec!("DisplayName", true, "Name to use to display in the editor"),
    spec!("Keywords", true, "Keywords this can be found by in the editor"),
    spec!("Category", true, "Category to list this under in the editor"),
    spec!("ToolTip", true, "Tooltip to show in the editor"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 表非空 + 结构合法（内容是取证产物，人工对账——不测内容正确性）。
    #[test]
    fn tables_nonempty_and_unique() {
        for (m, table) in [("UCLASS", UCLASS), ("UFUNCTION", UFUNCTION), ("UPROPERTY", UPROPERTY), ("UENUM", UENUM)] {
            assert!(!table.is_empty(), "{m} 表非空");
            let mut names: Vec<&str> = table.iter().map(|s| s.name).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), table.len(), "{m} 无重名");
        }
        assert!(specifiers_of("USTRUCT").is_empty(), "USTRUCT 引擎无消费，不给候选");
    }

    #[test]
    fn specifier_node_kind_to_macro() {
        assert_eq!(macro_of_specifier_node("uclass_specifiers"), Some("UCLASS"));
        assert_eq!(macro_of_specifier_node("ufunction_specifiers"), Some("UFUNCTION"));
        assert_eq!(macro_of_specifier_node("uproperty_specifiers"), Some("UPROPERTY"));
        assert_eq!(macro_of_specifier_node("uenum_specifiers"), Some("UENUM"));
        assert_eq!(macro_of_specifier_node("ustruct_specifiers"), None);
        assert_eq!(macro_of_specifier_node("umeta_specifiers"), None);
    }

    #[test]
    fn insert_respects_value_form() {
        let meta = UPROPERTY.iter().find(|s| s.name == "Meta").unwrap();
        assert_eq!(meta.insert(), "Meta=");
        let rw = UPROPERTY.iter().find(|s| s.name == "BlueprintReadWrite").unwrap();
        assert_eq!(rw.insert(), "BlueprintReadWrite");
    }
}
