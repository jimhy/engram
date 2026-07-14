//! 题库：护栏（inv）/ 偏好（pref）/ 质量（qual）/ 回放（replay）四类。
//!
//! 时间纪律：全 harness 禁用系统时钟，所有时刻都是相对锚点 [`T`] 的常量
//! （`T − n·DAY`），保证跨机器、跨时刻运行结果逐位一致。

pub mod inv;
pub mod pref;
pub mod qual;
pub mod replay;

/// 评测锚点时刻 now（unix 秒，任意固定常量，不取系统时钟）。
pub const T: f64 = 1_800_000_000.0;

/// 一天的秒数（引擎常量的转发，题目构造统一用它换算）。
pub const DAY: f64 = engram::model::SECS_PER_DAY;

/// 锚点前 `d` 天的时刻。
pub fn days_ago(d: f64) -> f64 {
    T - d * DAY
}

/// 题库骨架的规范化文本（冻结指纹的哈希对象，铁律 2 的机制化）。
///
/// 逐题列出 🔢 编码骨架的关键数字与断言。**修改任何题目构造时必须同步
/// 更新本文本**——题库冻结进 git 后，CI 以 `fingerprint` 输出比对记录在案
/// 的哈希，即可发现「跑完看结果回头改题」（p-hacking）。
pub const QUESTION_BANK_SPEC: &str = "\
engram-tuning-bank-v1
INV-01 高频胜稀疏 A=(L3,0,-5d,[-5,-3,-2,-1,-0.5]d) B=(L3,0,-5d,[-5d]) assert eff(A)>eff(B)
INV-02 近因胜久远 A=(L3,0,-0.5d,[-0.5d]) B=(L3,0,-50d,[-50d]) assert eff(A)>eff(B)
INV-03 单调衰减 m=(L3,0,-5d,[-5,-3,-1]d) assert eff(T)>eff(T+30d)>eff(T+100d)
INV-04 L1floor守得住 (L1,0.3,-400d,[-400d]) assert 仍L1且Active
INV-05 L3易忘 (L3,0,-500d,[-500d]) assert 转Cold
INV-06 迟滞不抖 m=(_,0.5,-30d,[-1d]x5) 起L3/L2 各5次consolidate(间隔2h) assert 0翻层
INV-07 频率门正常升 (L3,0.5,-8d,连续7天x3次/天) assert 升L2 (v0.9视界语义:跨日持续使用)
INV-08 grace容量护新 cap条(L3,0.3,-30d,[-10,-5]d)+E=(L3,0.3,-2d,[]) assert E仍Active
INV-09 grace不助上位 (L3,0.1,T,[-1d]) assert 不升不淘汰留L3
INV-10 溢出级联 cap+1条L1(imp各异) assert 恰cap条留L1,最低1条下推L2
INV-11 L4按项目计容 α=cap+1条L4.1 β=cap-1条 assert α推1条,β零下推
INV-12 pinned永驻 1条pinned L1(imp0,无访问)+cap条普通L1 assert pinned不降不推
INV-13 密钥轮换不误删 (Cold,0.4,-800d,[-800,-500,-200]d) assert gc=false
INV-14 纯噪声回收 (Cold,0.1,-200d,[-200d]) assert gc=true
INV-15 tombstone长寿 A=(Tomb,-1000d) B=(Tomb,-4000d) assert A留B删
INV-16 recall重置TTL (Cold,0.1,-800d,[-800,-10]d) assert gc=false
INV-17 分钟级采样红线 (L3,0.3,-5min,[-5min]) assert consolidate不升级(旧尖峰回归闸)
INV-18 溢出平局确定性 (a)promotion环 cap条(L1,0.5,-400d,[-400d])+loser=(L1,0.2,-399d,[-400d]) 3种构造序 assert 挤出恒为loser; (b)importance环 (cap-1)条(L1,0.9,-400d,[-1d])+keeper=(L1,0.5,-400d,[-1.5d])+victim=(L1,imp=promo(keeper),-401d,[-1d]) act(victim)=ln1=0故promotion逐位平 assert 挤出恒为victim; (c)created_at环 cap条(L1,0.5,-401d,[-400d])+young=(L1,0.5,-400d,[-400d]) 前三环逐位平 assert 挤出恒为young(更早创建者保留)
INV-19 重要度门 (L3,0.85,-10d,[])→L2且非L1; (L3,0.75)不动; (L2,0.85,-300d)不降
INV-20 字符预算溢出 (a)3条L2长cue(imp3.0/2.9/2.8,-1d,[-0.5d]) cue=B/2-100字符(B=l2.char_budget) 条数3<<cap 前置cost(3)>B且cost(2)<=B assert 字符预算触发溢出:最低eff者(w2)下推L3其余留L2; (b)单条L2 cue=B+500字符 assert 首条超预算仍留L2且零变迁(每层至少1条防饿死) 计量用memory_render_cost(与热索引渲染行同源)
PREF-01 纠错胜噪声 A=(L3,0.3,-600d,[])+Correct x3(-10,-7,-4)d B=(L3,0.1,-600d,[-598d]) reach偏序+eff严格区分
PREF-02 低频要命不被顶掉 A=(L3,0.8,-40d,[-30d]) B=(L3,0.1,-7d,[6次近期]) assert level(A)>=level(B) (v0.9门下预期Loss=0)
PREF-03 grace期内护新期满让位 A=(L3,0.1,-10d,[-9,-2]d) B=(L3,0.1,-10d,[]) reach偏序+eff严格区分
PREF-04 近期活跃胜久未问津 A=(L3,0,-90d,[-7,-3,-1]d) B=(L3,0,-90d,[-89d]) reach偏序+eff严格区分
PREF-05 重要度门空降 H=(L3,0.95,T,[])+10条普通L3 assert H入L2或更高 (v0.9门下预期Loss=0)
QUAL-01 token预算恒定性 混合负载N=300/180天/日巩固 loss=均值超额+0.5std+0.5正斜率(P=24000字符)
QUAL-02 效用加权命中率 未来真用前一刻可达度 Hot1/Cold0.5/Gone0 importance加权 loss=1-命中率
QUAL-03 噪声驻留时长 OneShot最后真用→转Cold平均天数(未转按存活期截尾) loss=天数
QUAL-04 层级恰当性 原型应然档vs实际档错配率(Evergreen应Hot;HotBug余温45d;OneShot30d后不应Hot;Seasonal/Refuted不应Gone)
QUAL-05 thrash 每次consolidate平均transition数
QUAL-06 满容乒乓率 imp>=gate被溢出挤下后又被门抬回次数/会话(只观测)
QUAL-07 节奏公平性 每2次机会用1次:周节奏(14d)vs日节奏(2d)各20条/imp0.5/180天 可达度差距(只观测)
REPLAY-01 前瞻命中 export快照按cutoff切分 hot/recall/lost率(只验不调,含幸存者偏差)
REPLAY-02 冷库召回覆盖 cutoff时已Cold且未来真用者 recall命中率(只验不调)
混合负载 Evergreen10%/HotBug30%/Seasonal5%/OneShot45%/Refuted10% 节律只用周期+抖动+泊松稀疏
Correct建模 +1真用+importance0.2封顶1.0
";

/// FNV-1a 64 位哈希（内联实现，避免依赖 `DefaultHasher` 的跨版本不稳定）。
fn fnv1a64(data: &str) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for b in data.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// 题库定义的规范化指纹（冻结进 git 后 CI 校验用）。
pub fn fingerprint() -> u64 {
    fnv1a64(QUESTION_BANK_SPEC)
}
