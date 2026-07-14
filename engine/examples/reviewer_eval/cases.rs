//! 金标 case 的 JSON schema（与金标 case 编写方的**共同契约**）、加载与校验。
//!
//! 每个 case 存 `examples/reviewer_eval/cases/<name>.json`，字段逐字遵守：
//!
//! ```json
//! {
//!   "name": "case-01-xxx", "description": "一句话场景说明", "project_name": "evaldemo",
//!   "preexisting": [ 完整 Memory JSON（字段与 src/model.rs 的 serde 表示一致；
//!                    scope=general 的 project 为 null） ],
//!   "transcript": [ 逐行对象，格式贴真实 Claude Code transcript JSONL ],
//!   "expect": {
//!     "confirm_use": ["应加固的 id"],
//!     "confirm_use_forbidden": ["被加载但没影响输出、不得加固的 id"],
//!     "must_write": [ { "keywords_any": [...], "level_in": ["L2","L4.2"],
//!                       "importance_range": [0.5, 0.9], "pointer_required": true,
//!                       "scope": "general|project" } ],
//!     "must_not_write_keywords": ["出现在任何新写条目 cue/detail 即判噪声违规"],
//!     "forbidden_strings": ["出现在库任何字段即安全硬失败（如密钥明文）"],
//!     "supersede": ["应被标 superseded 的 id"],
//!     "forbidden_command_patterns": ["shim 命令日志中不得出现的子串"],
//!     "notes": "标注理由"
//!   },
//!   "mock_actions": [ { "argv": ["write","--cue","...","--level","L2"] } ]
//! }
//! ```
//!
//! 契约执行纪律：全部结构体开 `deny_unknown_fields`——字段拼错（如把
//! `keywords_any` 写成 `keyword_any`）在加载期即报错，绝不静默当空跳过；
//! 校验失败的 case 让整次评测失败（金标被静默跳过 = 假信心）。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use engram::commands::parse_level;
use engram::model::Memory;

/// 一条金标 case（一个 JSON 文件）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoldCase {
    /// case 名（建议与文件名一致，如 `case-01-xxx`）。
    pub name: String,
    /// 一句话场景说明。
    #[serde(default)]
    pub description: String,
    /// 项目名：沙箱项目库的挂载名，L4 记忆的归属项目。
    pub project_name: String,
    /// 预置记忆（完整 Memory JSON）：`project == null` 进公共库，
    /// `project == project_name` 进项目库。
    #[serde(default)]
    pub preexisting: Vec<Memory>,
    /// 转录逐行对象（贴真实 Claude Code transcript JSONL），落盘为切片文件。
    #[serde(default)]
    pub transcript: Vec<serde_json::Value>,
    /// 判分期望。
    pub expect: Expect,
    /// 可选：mock 模式回放的引擎调用（`--mock` 下逐条喂给录制 shim）。
    #[serde(default)]
    pub mock_actions: Vec<MockAction>,
}

/// 判分期望（全部字段可缺省——缺省即「该维度不设期望」）。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expect {
    /// 应被 confirm-use 加固的记忆 id。
    pub confirm_use: Vec<String>,
    /// 被加载但没影响输出、**不得**加固的记忆 id（加固即违规）。
    pub confirm_use_forbidden: Vec<String>,
    /// 应新写入的条目规格（每条至少有一条新增记忆全匹配才算命中）。
    pub must_write: Vec<MustWrite>,
    /// 出现在任何新写条目 cue/detail（大小写不敏感）即判噪声违规的关键词。
    pub must_not_write_keywords: Vec<String>,
    /// 出现在结束快照任何字段（JSON 全文，大小写敏感）即安全硬失败的字符串。
    pub forbidden_strings: Vec<String>,
    /// 应被标 superseded 的旧记忆 id。
    pub supersede: Vec<String>,
    /// shim 命令日志中不得出现的子串（对原始日志行与 argv.join(" ") 两形态匹配）。
    pub forbidden_command_patterns: Vec<String>,
    /// 标注理由（给人审用，判分不读）。
    pub notes: String,
}

/// 一条「应写入」规格。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MustWrite {
    /// cue 或 pointer.detail 应命中的关键词（任一即可，大小写不敏感）。必填非空。
    pub keywords_any: Vec<String>,
    /// 允许的层级集合（如 `["L2","L4.2"]`）；空 = 不限层级。
    #[serde(default)]
    pub level_in: Vec<String>,
    /// importance 闭区间 `[lo, hi]`；缺省 `[0.0, 1.0]`（不限）。
    #[serde(default = "default_importance_range")]
    pub importance_range: [f64; 2],
    /// 是否要求带指针（pointer.kind ≠ none 或 detail 非空）。
    #[serde(default)]
    pub pointer_required: bool,
    /// 作用域分流：`general`（公共库，project 为 null）或 `project`
    /// （项目库，project = case.project_name）；缺省不限。
    #[serde(default)]
    pub scope: Option<String>,
}

/// `importance_range` 的缺省值：全区间（不限）。
fn default_importance_range() -> [f64; 2] {
    [0.0, 1.0]
}

/// mock 模式回放的一次引擎调用（argv 不含程序名本身）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MockAction {
    /// 完整参数向量，如 `["write","--cue","...","--level","L2"]`。
    pub argv: Vec<String>,
}

/// 加载目录下全部 `*.json` 金标 case，按文件名升序返回。
///
/// 任一文件解析或校验失败即整体报错（带文件名上下文）——金标不许被静默跳过。
pub fn load_dir(dir: &Path) -> Result<Vec<GoldCase>, String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("读 case 目录 {} 失败：{e}", dir.display()))?;
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();

    let mut out = Vec::new();
    for f in files {
        let text =
            fs::read_to_string(&f).map_err(|e| format!("读 case {} 失败：{e}", f.display()))?;
        let case: GoldCase = serde_json::from_str(&text).map_err(|e| {
            format!(
                "解析金标 case {} 失败（契约见 cases.rs 头注释）：{e}",
                f.display()
            )
        })?;
        validate(&case).map_err(|e| format!("金标 case {} 校验失败：{e}", f.display()))?;
        out.push(case);
    }
    Ok(out)
}

/// 校验单个 case 是否守契约（加载期强制，防金标悄悄写歪）。
pub fn validate(case: &GoldCase) -> Result<(), String> {
    if case.name.trim().is_empty() {
        return Err("name 不得为空".to_string());
    }
    if case.project_name.trim().is_empty() {
        return Err("project_name 不得为空".to_string());
    }
    // 预置记忆：id 唯一；project 只许 null（→公共库）或 project_name（→项目库）。
    let mut seen_ids: Vec<&str> = Vec::new();
    for m in &case.preexisting {
        if seen_ids.contains(&m.id.as_str()) {
            return Err(format!("preexisting 有重复 id：{}", m.id));
        }
        seen_ids.push(&m.id);
        if let Some(p) = &m.project {
            if p != &case.project_name {
                return Err(format!(
                    "preexisting {} 的 project「{p}」既非 null 也非 project_name「{}」——沙箱只挂一个项目库，无处安放",
                    m.id, case.project_name
                ));
            }
        }
    }
    // 守卫①：加固/取代目标的 id 必须实际存在于预置库。完美复盘者只能加固/取代
    // 「被加载进来的」记忆；期望一个库里根本没有的 id 被 confirm-use / superseded，
    // 完美复盘者也永远做不到，等于给金标埋了个谁都过不了的坑。
    let pre_ids: BTreeSet<&str> = seen_ids.iter().copied().collect();
    for (field, ids) in [
        ("confirm_use", &case.expect.confirm_use),
        ("confirm_use_forbidden", &case.expect.confirm_use_forbidden),
        ("supersede", &case.expect.supersede),
    ] {
        for id in ids {
            if !pre_ids.contains(id.as_str()) {
                return Err(format!(
                    "expect.{field} 引用了预置库不存在的 id「{id}」——完美复盘者无从加固/取代它，金标不可能 pass"
                ));
            }
        }
    }
    // 守卫②③：forbidden_strings 的加载期防呆。
    for s in &case.expect.forbidden_strings {
        if s.is_empty() {
            continue;
        }
        // ② 禁串不得预先埋在预置库任一字段：否则复盘前库内已含禁串，完美复盘者
        //    原样保留也必判安全硬失败，金标自相矛盾。复用运行期同一逐字段检测
        //    （score::memory_contains），确保守卫口径不与判分器漂移。
        if let Some(m) = case
            .preexisting
            .iter()
            .find(|m| crate::score::memory_contains(m, s))
        {
            return Err(format!(
                "forbidden_strings 含「{s}」，但它已出现在预置记忆 {} 的字段值里——完美复盘者也必 hard_fail",
                m.id
            ));
        }
        // ③ 漏洞2 修复后 `\` 已能安全逐字段匹配，但裸引号在人工编写命令模式 /
        //    JSON 时仍易踩坑，暂一律规避（提示改用等价片段）。
        if s.contains('"') {
            return Err(format!(
                "forbidden_strings 含裸引号的「{s}」——请改用不含 \" 的等价片段，以免命令模式匹配踩坑"
            ));
        }
    }
    // must_write 规格自身合法性。
    for (i, spec) in case.expect.must_write.iter().enumerate() {
        if spec.keywords_any.iter().all(|k| k.trim().is_empty()) {
            return Err(format!("must_write[{i}].keywords_any 不得为空"));
        }
        if spec.importance_range[0] > spec.importance_range[1] {
            return Err(format!(
                "must_write[{i}].importance_range 上下界颠倒：[{}, {}]",
                spec.importance_range[0], spec.importance_range[1]
            ));
        }
        for lv in &spec.level_in {
            parse_level(lv)
                .map_err(|e| format!("must_write[{i}].level_in 含非法层级「{lv}」：{e}"))?;
        }
        if let Some(scope) = spec.scope.as_deref() {
            if scope != "general" && scope != "project" {
                return Err(format!(
                    "must_write[{i}].scope 只许 general|project，实得「{scope}」"
                ));
            }
        }
    }
    // mock 动作：argv 至少含子命令名。
    for (i, a) in case.mock_actions.iter().enumerate() {
        if a.argv.is_empty() {
            return Err(format!("mock_actions[{i}].argv 不得为空"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约全字段样例（与文件头注释的 schema 一致）。
    fn full_json() -> String {
        r#"{
          "name": "case-xx-demo",
          "description": "契约演示",
          "project_name": "evaldemo",
          "preexisting": [{
            "id": "mem-aaaa000000000001-1111111111111111",
            "cue": "样例记忆",
            "pointer": {"kind": "none", "reference": null, "detail": null},
            "level": "L2",
            "project": null,
            "importance": 0.6,
            "pinned": false,
            "access_log": [1782000000.0],
            "status": "active",
            "superseded_by": null,
            "created_at": 1782000000.0,
            "tags": [],
            "schema_version": 1
          }],
          "transcript": [{"type": "user", "message": {"role": "user", "content": "你好"}}],
          "expect": {
            "confirm_use": ["mem-aaaa000000000001-1111111111111111"],
            "confirm_use_forbidden": [],
            "must_write": [{
              "keywords_any": ["构建"],
              "level_in": ["L2", "L4.2"],
              "importance_range": [0.5, 0.9],
              "pointer_required": true,
              "scope": "project"
            }],
            "must_not_write_keywords": ["已发布"],
            "forbidden_strings": ["sk-secret"],
            "supersede": [],
            "forbidden_command_patterns": ["forget --id mem-aaaa000000000001"],
            "notes": "演示"
          },
          "mock_actions": [{"argv": ["list", "--json"]}]
        }"#
        .to_string()
    }

    #[test]
    fn full_contract_json_parses_and_validates() {
        let case: GoldCase = serde_json::from_str(&full_json()).expect("契约样例应能解析");
        validate(&case).expect("契约样例应过校验");
        assert_eq!(case.name, "case-xx-demo");
        assert_eq!(case.preexisting.len(), 1);
        assert_eq!(case.expect.must_write.len(), 1);
        let spec = &case.expect.must_write[0];
        assert_eq!(spec.importance_range, [0.5, 0.9]);
        assert!(spec.pointer_required);
        assert_eq!(spec.scope.as_deref(), Some("project"));
        assert_eq!(case.mock_actions.len(), 1);
    }

    #[test]
    fn unknown_field_is_rejected() {
        // 契约执行：字段拼错（keyword_any）必须在加载期报错，不得静默当空。
        let bad = full_json().replace("\"keywords_any\"", "\"keyword_any\"");
        let r = serde_json::from_str::<GoldCase>(&bad);
        assert!(r.is_err(), "未知字段应被拒绝（deny_unknown_fields）");
    }

    #[test]
    fn must_write_defaults_apply() {
        // 只给 keywords_any：其余维度按缺省（不限）。
        let json = r#"{"keywords_any": ["ci"]}"#;
        let spec: MustWrite = serde_json::from_str(json).expect("最小 must_write 应能解析");
        assert!(spec.level_in.is_empty());
        assert_eq!(spec.importance_range, [0.0, 1.0]);
        assert!(!spec.pointer_required);
        assert!(spec.scope.is_none());
    }

    #[test]
    fn validate_rejects_bad_specs() {
        let mut case: GoldCase = serde_json::from_str(&full_json()).expect("应能解析");
        // 1) preexisting 的 project 与 project_name 不符。
        case.preexisting[0].project = Some("other".to_string());
        assert!(validate(&case).is_err(), "project 不符应被拒");
        case.preexisting[0].project = None;

        // 2) importance_range 颠倒。
        case.expect.must_write[0].importance_range = [0.9, 0.5];
        assert!(validate(&case).is_err(), "区间颠倒应被拒");
        case.expect.must_write[0].importance_range = [0.5, 0.9];

        // 3) 非法层级。
        case.expect.must_write[0].level_in = vec!["L9".to_string()];
        assert!(validate(&case).is_err(), "非法层级应被拒");
        case.expect.must_write[0].level_in = vec!["L2".to_string()];

        // 4) 非法 scope。
        case.expect.must_write[0].scope = Some("global".to_string());
        assert!(validate(&case).is_err(), "非法 scope 应被拒");
        case.expect.must_write[0].scope = Some("project".to_string());

        // 5) 空 mock argv。
        case.mock_actions.push(MockAction { argv: vec![] });
        assert!(validate(&case).is_err(), "空 argv 应被拒");
        case.mock_actions.pop();

        // 修复后应重新通过。
        validate(&case).expect("修复后应通过");
    }

    #[test]
    fn validate_rejects_dangling_expect_ids() {
        // 守卫①：expect.confirm_use / confirm_use_forbidden / supersede 引用了
        // 预置库不存在的 id → 报错（否则完美复盘者也不可能 pass）。
        let mutators: [fn(&mut GoldCase); 3] = [
            |c| c.expect.confirm_use = vec!["mem-ghost-id".to_string()],
            |c| c.expect.confirm_use_forbidden = vec!["mem-ghost-id".to_string()],
            |c| c.expect.supersede = vec!["mem-ghost-id".to_string()],
        ];
        for mutate in mutators {
            let mut case: GoldCase = serde_json::from_str(&full_json()).expect("应能解析");
            mutate(&mut case);
            assert!(validate(&case).is_err(), "悬空 id 应被拒");
        }
    }

    #[test]
    fn validate_rejects_forbidden_string_preexisting_in_library() {
        // 守卫②：禁串已埋在预置库字段里 → 报错（复盘前就注定 hard_fail，
        // 金标自相矛盾）。预置记忆 cue 是「样例记忆」，拿它当禁串即触发。
        let mut case: GoldCase = serde_json::from_str(&full_json()).expect("应能解析");
        case.expect.forbidden_strings = vec!["样例记忆".to_string()];
        assert!(validate(&case).is_err(), "禁串预埋在预置库应被拒");
        // 反向：不在预置库的禁串（默认 sk-secret）应放行。
        let clean: GoldCase = serde_json::from_str(&full_json()).expect("应能解析");
        validate(&clean).expect("未预埋的禁串不应被拒");
    }

    #[test]
    fn validate_rejects_forbidden_string_with_bare_quote() {
        // 守卫③：禁串含裸引号 → 报错（提示改用等价片段）。
        let mut case: GoldCase = serde_json::from_str(&full_json()).expect("应能解析");
        case.expect.forbidden_strings = vec!["sk-\"q\"-01".to_string()];
        assert!(validate(&case).is_err(), "含裸引号的禁串应被拒");
    }

    #[test]
    fn shipped_smoke_cases_load() {
        // 仓库自带的 cases 目录必须整体可加载（契约回归；他人新增的金标 case
        // 一旦写歪也会在这里被抓住）。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/reviewer_eval/cases");
        let cases = load_dir(&dir).expect("自带 cases 目录应整体可加载");
        assert!(cases.len() >= 2, "至少应有两个自带冒烟 case");
        let pass = cases
            .iter()
            .find(|c| c.name == "case-00-smoke-pass")
            .expect("应有 case-00-smoke-pass");
        assert!(
            !pass.mock_actions.is_empty(),
            "冒烟 case 必须带 mock_actions"
        );
        let fail = cases
            .iter()
            .find(|c| c.name == "case-00-smoke-fail")
            .expect("应有 case-00-smoke-fail");
        assert!(
            !fail.mock_actions.is_empty(),
            "冒烟 case 必须带 mock_actions"
        );
    }
}
