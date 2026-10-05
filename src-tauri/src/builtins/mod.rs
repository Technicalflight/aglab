//! 出厂扩展（design-builtin-extensions.md）：定义在代码里、随应用自带的能力包，
//! 用户配置只存偏离（`disabled_builtins`）。正文用 `include_str!` 编进二进制——
//! 落成磁盘文件就会和版本漂移，应用升级后没人能解释「为什么改了没生效」。
//!
//! v1 的扩展只带技能：技能是提示词清单，模型按描述自行取用，零新增攻击面。

/// 出厂扩展带的一条技能。与磁盘技能同构，只是没有文件——
/// `path` 留空，`load_skill` 从这里给的正文直接返回
pub struct BuiltinSkill {
    /// 技能目录名。全局键 = `内置/<扩展id>/<folder>`（见 [`BuiltinExtension::skill_key`]）
    pub folder: &'static str,
    pub name: &'static str,
    /// 模型决定取不取的唯一依据，写场景不写口号
    pub description: &'static str,
    /// 工具白名单，空 = 不额外限制。测试钉住：只许写注册表里真实存在的工具 id
    pub allowed_tools: &'static [&'static str],
    pub body: &'static str,
}

/// 一条出厂扩展
pub struct BuiltinExtension {
    /// 保留 id，小写连字符。界面开关与 `disabled_builtins` 都用它
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub skills: Vec<BuiltinSkill>,
}

impl BuiltinExtension {
    /// 技能的全局键。与个人/插件技能共用 `disabled_skills` 键空间，
    /// `内置/` 前缀把出厂技能和用户技能隔开
    pub fn skill_key(&self, folder: &str) -> String {
        format!("内置/{}/{folder}", self.id)
    }
}

/// 出厂名册。升级即更新——这里改了，下一版所有用户的这份技能就是新的
pub fn extensions() -> Vec<BuiltinExtension> {
    vec![
        BuiltinExtension {
            id: "guide",
            name: "aglab 使用指南",
            description: "全应用导览与按症状排查：设置各页、模型池与路由、权限档位、子助理、技能插件 MCP 钩子、任务编排。",
            skills: vec![
                BuiltinSkill {
                    folder: "guide",
                    name: "aglab 使用指南",
                    description: "用户问 aglab 本身怎么用、怎么配置（服务商档案、模型池、模型路由、代理、权限档位、子助理、技能、插件、MCP、钩子、任务、编排、用量、缓存）时使用",
                    allowed_tools: &[],
                    body: include_str!("guide.md"),
                },
                BuiltinSkill {
                    folder: "diagnostics",
                    name: "故障诊断",
                    description: "aglab 出了问题按症状排查时使用：发送报错、技能没生效、MCP 连不上、钩子不跑、插件不出现、出口被拦、用量对不上",
                    allowed_tools: &[],
                    body: include_str!("diagnostics.md"),
                },
            ],
        },
        BuiltinExtension {
            id: "skill-forge",
            name: "技能创建器",
            description: "教模型替用户写出能被真正用起来的 aglab 技能：目录、frontmatter、正文的写法与验证路径。",
            skills: vec![BuiltinSkill {
                folder: "skill-forge",
                name: "技能创建器",
                description: "用户想创建、修改或改进 aglab 技能（SKILL.md），问技能怎么写、为什么没被用上时使用",
                allowed_tools: &[],
                body: include_str!("skill-forge.md"),
            }],
        },
        BuiltinExtension {
            id: "plugin-forge",
            name: "插件创建器",
            description: "教模型替用户打 aglab 插件包：manifest、技能、.mcp.json、钩子与信任机制、验证清单。",
            skills: vec![BuiltinSkill {
                folder: "plugin-forge",
                name: "插件创建器",
                description: "用户想创建或修改 aglab 插件（plugin.json、.mcp.json、hooks.json），问插件目录格式与排错时使用",
                allowed_tools: &[],
                body: include_str!("plugin-forge.md"),
            }],
        },
        // ── 第二批（design-builtin-extensions.md §2.1）：文档与自动化。
        // 剧本型技能：教模型驱动本机已有的运行时（Python + 各格式库）或已有的工具
        //（web_fetch、窗口三件套），不新造执行通道
        BuiltinExtension {
            id: "doc-docx",
            name: "Word 文档",
            description: "创建、编辑与审阅 Word 文档（DOCX）：python-docx 剧本、跨 run 替换的坑、备份红线。",
            skills: vec![BuiltinSkill {
                folder: "docx",
                name: "Word 文档",
                description: "用户要创建、修改或审阅 .docx 文档，问 Word 文档怎么生成/改内容/提取文本时使用",
                allowed_tools: &[],
                body: include_str!("doc-docx.md"),
            }],
        },
        BuiltinExtension {
            id: "doc-xlsx",
            name: "电子表格",
            description: "创建、编辑与审阅电子表格（XLSX）：openpyxl 剧本、公式与缓存值陷阱、xlsm 宏保留。",
            skills: vec![BuiltinSkill {
                folder: "xlsx",
                name: "电子表格",
                description: "用户要创建、修改或审阅 .xlsx/.xlsm 表格、批量填数、加图表、盘点表内容时使用",
                allowed_tools: &[],
                body: include_str!("doc-xlsx.md"),
            }],
        },
        BuiltinExtension {
            id: "doc-pptx",
            name: "演示文档",
            description: "创建、编辑与审阅演示文档（PPTX）：python-pptx 剧本、版式与占位符、大纲导出。",
            skills: vec![BuiltinSkill {
                folder: "pptx",
                name: "演示文档",
                description: "用户要创建、修改或审阅 .pptx 幻灯片、按模板生成、导出大纲时使用",
                allowed_tools: &[],
                body: include_str!("doc-pptx.md"),
            }],
        },
        BuiltinExtension {
            id: "doc-pdf",
            name: "PDF",
            description: "创建、编辑与审阅 PDF：pypdf 读取与组装、reportlab/fpdf2 生成与中文字体、扫描件的诚实交代。",
            skills: vec![BuiltinSkill {
                folder: "pdf",
                name: "PDF",
                description: "用户要读取/合并/拆分 PDF、从零生成 PDF、提取 PDF 文本审阅时使用",
                allowed_tools: &[],
                body: include_str!("doc-pdf.md"),
            }],
        },
        BuiltinExtension {
            id: "image-search",
            name: "搜图",
            description: "查找插图与参考配图：复用读网页工具查免密钥的开放图库 API，授权与署名三件套照办。",
            skills: vec![BuiltinSkill {
                folder: "image-search",
                name: "搜图",
                description: "用户要找插图、配图、素材图，问有没有符合主题的图片时使用",
                allowed_tools: &[],
                body: include_str!("image-search.md"),
            }],
        },
        BuiltinExtension {
            id: "desktop-auto",
            name: "桌面自动化",
            description: "窗口三件套（列窗口 → 读控件 → 合成输入）的实操剧本：先探后动、每步验证、红线复述。",
            skills: vec![BuiltinSkill {
                folder: "desktop-auto",
                name: "桌面自动化",
                description: "要替用户操作本机其他程序（填表单、点按钮、驱动没有命令行界面的应用）时使用",
                allowed_tools: &[],
                body: include_str!("desktop-auto.md"),
            }],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名册自身的不变式：id 是小写连字符 slug、全局不重；每条扩展至少带一个技能，
    /// 技能目录名在扩展内不重；正文非空且以标题开头（列表预览靠它）。
    /// 白名单只许写注册表里真实存在的工具 id——出厂内容写错工具名，闸门就名存实亡
    #[test]
    fn the_builtin_roster_is_well_formed() {
        let mut seen_ids = Vec::new();
        for extension in extensions() {
            assert!(
                !extension.id.is_empty()
                    && extension
                        .id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-'),
                "扩展 id 应是小写连字符：{}",
                extension.id
            );
            assert!(!seen_ids.contains(&extension.id), "扩展 id 重复：{}", extension.id);
            seen_ids.push(extension.id);
            assert!(!extension.name.trim().is_empty());
            assert!(!extension.description.trim().is_empty());
            assert!(
                !extension.skills.is_empty(),
                "扩展 {} 一条技能都不带就不是扩展，是占位",
                extension.id
            );

            let mut seen_folders = Vec::new();
            for skill in &extension.skills {
                assert!(
                    !seen_folders.contains(&skill.folder),
                    "扩展 {} 的技能目录名重复：{}",
                    extension.id,
                    skill.folder
                );
                seen_folders.push(skill.folder);
                assert!(!skill.name.trim().is_empty());
                assert!(
                    skill.description.len() > 10,
                    "描述写场景不写口号：{}",
                    skill.name
                );
                assert!(skill.body.trim_start().starts_with('#'), "{} 缺标题", skill.name);
                for tool in skill.allowed_tools {
                    assert!(
                        crate::tools::is_registered(tool),
                        "扩展 {} 的技能 {} 白名单里有不存在的工具：{}",
                        extension.id,
                        skill.name,
                        tool
                    );
                }
            }
        }
        assert!(seen_ids.len() >= 3, "出厂至少要有一手可用的话：{}", seen_ids.len());
    }

    /// 键空间：`内置/<扩展id>/<目录名>`。前缀把出厂技能与用户技能隔开，
    /// 扩展 id 里没有 `/`，键不会歧义
    #[test]
    fn builtin_skill_keys_are_namespaced() {
        for extension in extensions() {
            for skill in &extension.skills {
                let key = extension.skill_key(skill.folder);
                assert!(key.starts_with("内置/"), "{key}");
                assert_eq!(
                    key.split('/').count(),
                    3,
                    "键是三段式，来源标签取第一段才是「内置」：{key}"
                );
            }
        }
    }
}
