//! 确定性随机数：内联 xorshift64*，零外部依赖。
//!
//! 全 harness 禁用系统时钟与操作系统随机源——种子显式传入、序列完全确定，
//! 保证「同种子同结果」可复现（方案 §5 铁律 3 的前提）。

/// xorshift64* 伪随机数发生器。
pub struct XorShift64Star {
    /// 内部状态（非零）。
    state: u64,
}

impl XorShift64Star {
    /// 用显式种子构造；种子为 0 时用固定常量替代（xorshift 状态不允许全零）。
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// 下一个 64 位伪随机数。
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, 1)` 均匀浮点。
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `[lo, hi)` 均匀浮点。
    pub fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    /// `[lo, hi]` 均匀整数（闭区间）。
    pub fn range_usize(&mut self, lo: usize, hi: usize) -> usize {
        debug_assert!(lo <= hi, "range_usize 要求 lo <= hi");
        lo + (self.next_u64() % (hi - lo + 1) as u64) as usize
    }

    /// 以概率 `p` 返回 true（泊松式稀疏事件的基本件）。
    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}
