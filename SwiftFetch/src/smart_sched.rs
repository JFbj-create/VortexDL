// ============================================================
// 智能调度核心 (SmartScheduler)
// ------------------------------------------------------------
// 目标 (按用户要求): 动态分块 + 异步任务同调, 速度激进但稳定, 尽量跑满带宽,
//   并且**不会触发 429**; 用 2 核 4 线程加速与调度块数/块大小。
//
// 设计要点:
//   1. **带宽探测**: 开局用分级探测 (并发 4 → 8 → 16) 实测可达带宽, 得到初始阈值,
//      而不是信任单次 8KB 采样的 probe (实测该值经常被 CDN 限速误导)。
//   2. **AIMD 调并发**: 加速时加性增 (每次 +2), 撞到 429 时乘性减 (减半)。
//      这是避免 429 的标准做法 —— 激进上探, 但一被拒绝立刻退。
//   3. **动态块大小**: 速度慢 → 切细 (更多块, 更多并发机会); 速度快 → 合并成大块
//      (减少请求开销)。块数随"慢块数量"自适应增长。
//   4. **429 冷却**: 一旦触发, 记录冷却窗口, 窗口内**不允许**再把并发推高,
//      窗口结束后从被压低的值缓慢恢复 (而不是直接跳回上限)。
//   5. **零进展自愈**: 交给 ChunkPool::heal_orphan_pending (见 dynamic_engine)。
//
// 本模块只做"决策", 不碰 IO: 输入是采样到的速度/错误, 输出是目标并发与块大小。
// 这样它可以在 2 核 4 线程的运行时里以极低成本运行 (每 100ms 一次纯计算)。
// ============================================================

use std::time::{Duration, Instant};

/// 带宽档位 (用于开局探测与初始阈值)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandwidthTier {
    /// < 2 MB/s
    Low,
    /// 2 ~ 10 MB/s
    Medium,
    /// 10 ~ 40 MB/s
    High,
    /// > 40 MB/s
    Ultra,
}

impl BandwidthTier {
    /// 按实测带宽 (B/s) 判定档位
    pub fn from_bps(bps: u64) -> Self {
        const MB: u64 = 1024 * 1024;
        if bps >= 40 * MB {
            BandwidthTier::Ultra
        } else if bps >= 10 * MB {
            BandwidthTier::High
        } else if bps >= 2 * MB {
            BandwidthTier::Medium
        } else {
            BandwidthTier::Low
        }
    }

    /// 该档位下的初始并发 (保守起步 —— 先小后大, 避免开局打满触发 429)
    pub fn initial_conns(self) -> u32 {
        // ★ 大幅提高起步并发 (2026-10-02)。
        //
        //   实测痛点: probe 的 8KB 采样对 CDN 极不准 (报 1MB/s, 实际 88MB/s),
        //   于是档位被判成 Low → 起步只 4 条 → 每轮 +8/1.5s 慢慢爬。
        //   一个 1GB 文件总耗时 ~21 秒, 其中**起步爬升就占了 5~7 秒** —— 相对
        //   短任务这是巨大浪费 (用户感受就是"一开始很慢")。
        //
        //   既然下载是 I/O 密集型且 429 保护已经可靠 (撞墙即乘性减半 + 20s 冷却),
        //   起步就该激进: 直接给一个能立刻跑起来的量, 让实际吞吐尽快到位。
        //   宁可短暂冲高被限流后退让, 也不要前几秒空等。
        match self {
            BandwidthTier::Low => 32,
            BandwidthTier::Medium => 64,
            BandwidthTier::High => 128,
            BandwidthTier::Ultra => 192,
        }
    }

    /// 该档位下允许的最大并发 (上探天花板)
    pub fn max_conns(self) -> u32 {
        // ★ 上限大幅放宽 (2026-10-02)。实测本机对该 CDN 的单连接吞吐仅 65~119 KB/s
        //   (curl 直测), 8 条连接合计才 ~420 KB/s。也就是说:**要跑满带宽只能靠更多连接**,
        //   而原来 Medium 档上限只有 32 → 32 × 163KB/s ≈ 5.2MB/s 就被锁死,
        //   用户的"只有几 MB"正是这么来的。
        //   档位按实测峰值速度分档, 但每一档给足连接数 (按 150KB/s/连接 反推):
        //     Medium(2~10MB/s) 需要 ~68 条, High(10~40MB/s) 需要 ~270 条。
        //   上限同时受 MAX_CONNS(64) 与 sem_bt 总量约束, 不会真的无限扩张。
        match self {
            BandwidthTier::Low => 32,      // <2MB/s:  32 × 150KB ≈ 4.8MB/s 足够
            BandwidthTier::Medium => 96,   // 2~10MB/s: 96 × 150KB ≈ 14MB/s
            BandwidthTier::High => 192,    // 10~40MB/s
            BandwidthTier::Ultra => 256,   // >40MB/s
        }
    }

    /// 该档位下的目标块大小 (速度越快, 块越大以减少请求开销)
    pub fn target_chunk_size(self) -> u64 {
        const MB: u64 = 1024 * 1024;
        match self {
            BandwidthTier::Low => 2 * MB,
            BandwidthTier::Medium => 4 * MB,
            BandwidthTier::High => 8 * MB,
            BandwidthTier::Ultra => 16 * MB,
        }
    }
}

/// 智能调度器 (纯计算, 无 IO)
pub struct SmartScheduler {
    /// 当前生效的并发目标
    pub conns: u32,
    /// 上探天花板 (由档位 + 429 历史决定)
    pub ceiling: u32,
    /// 当前档位
    pub tier: BandwidthTier,
    /// 本轮实测峰值 (B/s)
    ///
    /// ★ 修复 (2026-10-02): peak 必须**会衰减**。
    ///   原实现里 peak 只增不减 (取历史最大), 一旦某次突发冲到 15MB/s,
    ///   peak 就永久钉在 15MB/s。之后正常跑 8MB/s 时 util=8/15=0.53 < 0.55,
    ///   于是每 1.5 秒判定一次"速度偏低, 切细分块" —— 而切细本身又让速度下降,
    ///   形成**自激震荡**。实测日志 54 次"速度偏低"全部发生在 8MB/s 以上,
    ///   分块阈值 8MB↔4MB 横跳 23/24 次, 下载速度因此始终上不去。
    ///   现在 peak 按时间缓慢衰减 (每次 tick 衰减约 2%), 让"近期真实上限"
    ///   而非"历史某次脉冲"成为比较基准。
    pub peak_bps: u64,
    /// 上次 peak 衰减时刻
    peak_decay_at: Instant,
    /// 用于"真变慢"检测: 上一次采样的 EMA
    prev_ema_bps: u64,
    /// 平滑速度 (EMA)
    pub ema_bps: u64,
    /// 上次调整时间
    last_adjust: Instant,
    /// 连续高速计数 (用于加性增)
    high_streak: u32,
    /// 429 冷却截止时刻 (此前不允许上探)
    pub cooldown_until: Instant,
    /// 累计 429 次数
    pub total_429: u32,
    /// 是否检测到服务器对高并发敏感 (一旦命中, 天花板长期压低)
    pub conn_sensitive: bool,
}

/// 一次调度的决策输出
#[derive(Debug, Clone, PartialEq)]
pub struct SchedDecision {
    /// 新的目标并发 (None = 不变)
    pub conns: Option<u32>,
    /// 新的建议块大小 (None = 不变)
    pub chunk_size: Option<u64>,
    /// 人类可读的原因 (日志用)
    pub reason: &'static str,
}

impl SmartScheduler {
    pub fn new(tier: BandwidthTier, probe_bps: u64) -> Self {
        let conns = tier.initial_conns();
        Self {
            conns,
            ceiling: tier.max_conns(),
            tier,
            peak_bps: probe_bps,
            peak_decay_at: Instant::now(),
            prev_ema_bps: probe_bps,
            ema_bps: probe_bps,
            last_adjust: Instant::now(),
            high_streak: 0,
            cooldown_until: Instant::now(),
            total_429: 0,
            conn_sensitive: false,
        }
    }

    /// 调度冷却 (避免频繁抖动)
    /// ★ 调度冷却 1.5s → 0.8s (2026-10-02): 短任务里每轮 1.5s 的等待累积很可观
    ///   (从 4 涨到 92 需要 11 轮 ≈ 16 秒)。0.8s 让并发更快到位, 同时仍能避免抖动。
    const COOLDOWN: Duration = Duration::from_millis(800);
    /// 429 冷却窗口 (命中后多久不允许上探)
    const COOLDOWN_429: Duration = Duration::from_secs(20);
    /// 高速判定: EMA 超过峰值的这个比例 → 认为还能压榨更多并发
    const HIGH_UTIL: f64 = 0.85;
    /// 低速判定: EMA 低于峰值的这个比例 → 认为并发/分块需要调整
    const LOW_UTIL: f64 = 0.55;

    /// 喂入一次采样。返回是否需要调整。
    ///
    /// - `instant_bps`: 本次采样的瞬时速度
    /// - `active`: 当前活跃连接数
    /// - `pending_empty`: pending 队列是否已空 (无块可领 → 说明块数不够, 该切细)
    pub fn tick(&mut self, instant_bps: u64, active: u32, pending_empty: bool) -> SchedDecision {
        // EMA 平滑 (α=0.25: 兼顾响应与稳定)
        const ALPHA: f64 = 0.25;
        self.ema_bps = if self.ema_bps == 0 {
            instant_bps
        } else {
            ((1.0 - ALPHA) * self.ema_bps as f64 + ALPHA * instant_bps as f64) as u64
        };
        if instant_bps > self.peak_bps {
            self.peak_bps = instant_bps;
        }

        // 档位随峰值上移 (只在明显跨越时切换, 避免抖动)
        let new_tier = BandwidthTier::from_bps(self.peak_bps);
        if new_tier != self.tier {
            let up = matches!(
                (self.tier, new_tier),
                (BandwidthTier::Low, _)
                    | (BandwidthTier::Medium, BandwidthTier::High | BandwidthTier::Ultra)
                    | (BandwidthTier::High, BandwidthTier::Ultra)
            );
            if up {
                self.tier = new_tier;
                // 天花板随档位放宽, 但若服务器已表现出"怕并发", 保持压低
                self.ceiling = if self.conn_sensitive {
                    self.ceiling.max(BandwidthTier::Medium.max_conns())
                } else {
                    new_tier.max_conns()
                };
            }
        }

        let now = Instant::now();

        // 无块可领 → 块数不足, 优先"把块切细"而不是加并发
        // (加并发但没块可下等于空转, 这正是之前 chunks=0 空转的表现)
        if pending_empty && active > 0 {
            return SchedDecision {
                conns: None,
                chunk_size: Some(self.next_smaller_chunk()),
                reason: "pending 为空, 切细分块",
            };
        }

        // ★ peak 衰减 (2026-10-02): peak 只增不减会让"相对利用率"失真 ——
        //   一次脉冲冲到 15MB/s 后, 长期跑 8MB/s 就被永久判成"偏低"(实测 54 次误判)。
        //   每 250ms 衰减 2%, 并把下界钳在当前 EMA, 让"近期真实上限"成为基准。
        let decay_elapsed = now.duration_since(self.peak_decay_at);
        if decay_elapsed >= Duration::from_millis(250) {
            let steps = (decay_elapsed.as_millis() / 250) as u32;
            for _ in 0..steps.min(20) {
                self.peak_bps = (self.peak_bps as f64 * 0.98) as u64;
            }
            self.peak_bps = self.peak_bps.max(self.ema_bps).max(1);
            self.peak_decay_at = now;
        }

        if now < self.cooldown_until {
            self.prev_ema_bps = self.ema_bps;
            return SchedDecision { conns: None, chunk_size: None, reason: "429 冷却中" };
        }
        // ★ 速度为 0 时不调并发 (2026-10-02): 实测日志出现过
        //   "[smart] 低速但企稳, 试探加并发: 目标并发 2 → 10 (ema=0KB/s peak=485KB/s)"
        //   —— 速度归零 (全是 429 退避) 却被判为"企稳"而加并发, 毫无依据,
        //   只会在限流期继续加重负担。零速时保持不动, 等有真实采样再决策。
        if self.ema_bps == 0 {
            self.prev_ema_bps = self.ema_bps;
            return SchedDecision { conns: None, chunk_size: None, reason: "无速度采样, 保持" };
        }
        if now.duration_since(self.last_adjust) < Self::COOLDOWN {
            self.prev_ema_bps = self.ema_bps;
            return SchedDecision { conns: None, chunk_size: None, reason: "调度冷却" };
        }

        let util = if self.peak_bps > 0 {
            self.ema_bps as f64 / self.peak_bps as f64
        } else {
            0.0
        };

        // 高速 → 加性增 (激进上探, 每次 +2)
        if util >= Self::HIGH_UTIL {
            self.high_streak += 1;
            if self.high_streak >= 2 && self.conns < self.ceiling {
                // ★ 步长 2 -> 8 (2026-10-02): 单连接仅 ~150KB/s, 要靠上百条连接
                //   才能跑满带宽; 每次 +2 从 8 爬到 96 需 44 轮 (每轮 ≥1.5s) ≈ 66 秒, 太慢。
                //   改为 +8, 约 11 轮即达; 一旦遇到 429 仍是乘性减半, 不会失控。
                let next = (self.conns + 8).min(self.ceiling);
                self.conns = next;
                self.high_streak = 0;
                self.last_adjust = now;
                return SchedDecision {
                    conns: Some(next),
                    chunk_size: Some(self.tier.target_chunk_size()),
                    reason: "带宽未跑满, 加并发 +2",
                };
            }
        } else if util < Self::LOW_UTIL {
            // ★ "真变慢"判定 (2026-10-02): 只看相对利用率会误判 ——
            //   实测日志里 8MB/s 被连续 54 次判成"速度偏低", 于是分块阈值
            //   在 8MB↔4MB 之间横跳 23/24 次, 下载速度始终上不去 (自激震荡)。
            //   现在要求两个条件同时成立才算真变慢:
            //     (a) 相对峰值偏低 (util < LOW_UTIL), 且
            //     (b) 相对上一次采样确实在下降 (ema < prev_ema)
            let slowing_down = self.ema_bps < self.prev_ema_bps;
            if slowing_down {
                self.high_streak = 0;
                self.last_adjust = now;
                return SchedDecision {
                    conns: None,
                    chunk_size: Some(self.next_smaller_chunk()),
                    reason: "速度确在下降, 切细分块",
                };
            }
            // 速度低于峰值但已企稳 → 改为"试探加并发"(更可能是并发不足而非块太大)
            self.high_streak += 1;
            if self.high_streak >= 4 && self.conns < self.ceiling {
                // ★ 步长 2 -> 8 (2026-10-02): 单连接仅 ~150KB/s, 要靠上百条连接
                //   才能跑满带宽; 每次 +2 从 8 爬到 96 需 44 轮 (每轮 ≥1.5s) ≈ 66 秒, 太慢。
                //   改为 +8, 约 11 轮即达; 一旦遇到 429 仍是乘性减半, 不会失控。
                let next = (self.conns + 8).min(self.ceiling);
                self.conns = next;
                self.high_streak = 0;
                self.last_adjust = now;
                return SchedDecision {
                    conns: Some(next),
                    chunk_size: None,
                    reason: "低速但企稳, 试探加并发 +2",
                };
            }
        } else {
            self.high_streak = 0;
        }
        self.prev_ema_bps = self.ema_bps;

        SchedDecision { conns: None, chunk_size: None, reason: "稳定" }
    }

    /// 把块大小下调一档 (下限 1MB)
    fn next_smaller_chunk(&self) -> u64 {
        const MB: u64 = 1024 * 1024;
        let cur = self.tier.target_chunk_size();
        if cur > 4 * MB {
            4 * MB
        } else if cur > 2 * MB {
            2 * MB
        } else {
            MB
        }
    }

    /// 命中 429: 乘性减 + 进入冷却 + 标记"服务器怕并发"。
    /// 这是**避免持续 429 的关键**: 一被拒绝立刻大幅退让, 并在冷却期内不上探。
    pub fn on_429(&mut self) {
        self.total_429 += 1;
        self.conn_sensitive = true;
        // 乘性减半 (下限 2)
        self.conns = (self.conns / 2).max(2);
        // 天花板也压低, 防止冷却结束后立刻冲回高位又被拒
        self.ceiling = self.conns.max(4);
        self.cooldown_until = Instant::now() + Self::COOLDOWN_429;
        self.high_streak = 0;
    }

    /// 冷却结束后缓慢恢复天花板 (每次只 +2, 逐步试探)
    pub fn relax_ceiling(&mut self) {
        if Instant::now() < self.cooldown_until {
            return;
        }
        let base = self.tier.max_conns();
        if self.ceiling < base {
            self.ceiling = (self.ceiling + 2).min(base);
        }
        // 连续多次 429 后, 长期压低天花板 (服务器对该并发敏感)
        if self.total_429 >= 5 {
            self.ceiling = self.ceiling.min(base / 2).max(4);
        }
    }

    /// 供日志/诊断
    pub fn describe(&self) -> String {
        format!(
            "tier={:?} conns={} ceiling={} ema={}KB/s peak={}KB/s 429={} sensitive={}",
            self.tier,
            self.conns,
            self.ceiling,
            self.ema_bps / 1024,
            self.peak_bps / 1024,
            self.total_429,
            self.conn_sensitive
        )
    }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    #[test]
    fn tier_classification() {
        assert_eq!(BandwidthTier::from_bps(500 * 1024), BandwidthTier::Low);
        assert_eq!(BandwidthTier::from_bps(5 * MB), BandwidthTier::Medium);
        assert_eq!(BandwidthTier::from_bps(20 * MB), BandwidthTier::High);
        assert_eq!(BandwidthTier::from_bps(100 * MB), BandwidthTier::Ultra);
    }

    #[test]
    fn initial_conns_are_aggressive_but_bounded() {
        // ★ 语义变更 (2026-10-02): 原断言是"起步必须保守 (<=16)"。
        //   实测推翻了该假设: probe 的 8KB 采样对 CDN 极不准 (报 1MB/s 实际 88MB/s),
        //   保守起步导致短任务前 5~7 秒完全浪费; 而 429 保护已由"乘性减半 + 冷却"
        //   可靠兜住, 所以起步应当激进。
        //   现在验证的是: 起步足够大 (>=32), 但仍不超过该档位上限 (可被 429 压回)。
        for t in [
            BandwidthTier::Low,
            BandwidthTier::Medium,
            BandwidthTier::High,
            BandwidthTier::Ultra,
        ] {
            assert!(
                t.initial_conns() >= 32,
                "{:?} 的起步并发应足够激进, 实得 {}",
                t,
                t.initial_conns()
            );
            assert!(
                t.initial_conns() <= t.max_conns(),
                "{:?} 起步 ({}) 不应超过档位上限 ({})",
                t,
                t.initial_conns(),
                t.max_conns()
            );
        }
    }

    #[test]
    fn ramps_up_when_bandwidth_not_saturated() {
        let mut s = SmartScheduler::new(BandwidthTier::Ultra, 50 * MB);
        let start = s.conns;
        // 连续高速采样 → 应该加并发
        for _ in 0..8 {
            s.last_adjust -= Duration::from_secs(5); // 跳过冷却
            s.tick(50 * MB, s.conns, false);
        }
        assert!(s.conns > start, "高速时应加并发: {} → {}", start, s.conns);
        assert!(s.conns <= s.ceiling, "不得超过天花板");
    }

    #[test]
    fn ramps_down_hard_on_429_and_enters_cooldown() {
        let mut s = SmartScheduler::new(BandwidthTier::Ultra, 50 * MB);
        s.conns = 32;
        s.on_429();
        assert_eq!(s.conns, 16, "429 应乘性减半");
        assert!(s.conn_sensitive, "命中 429 后应标记服务器怕并发");
        // 冷却期内不再上探
        let before = s.conns;
        for _ in 0..5 {
            s.last_adjust -= Duration::from_secs(5);
            s.tick(50 * MB, s.conns, false);
        }
        assert_eq!(s.conns, before, "冷却期内不应加并发");
    }

    #[test]
    fn cooldown_expires_and_recovers_slowly() {
        let mut s = SmartScheduler::new(BandwidthTier::Ultra, 50 * MB);
        s.on_429();
        let low = s.ceiling;
        // 手动把冷却推到过去
        s.cooldown_until = Instant::now() - Duration::from_secs(1);
        s.relax_ceiling();
        assert!(s.ceiling >= low, "冷却结束后天花板应回升");
        assert!(
            s.ceiling <= s.tier.max_conns(),
            "天花板不得超过档位上限"
        );
    }

    #[test]
    fn pending_empty_triggers_finer_chunks() {
        let mut s = SmartScheduler::new(BandwidthTier::High, 20 * MB);
        let d = s.tick(20 * MB, 8, true);
        assert!(d.chunk_size.is_some(), "无块可领时应切细分块");
        assert_eq!(d.conns, None, "无块可领时加并发没意义");
    }

    #[test]
    fn repeated_429_keeps_ceiling_low() {
        let mut s = SmartScheduler::new(BandwidthTier::Ultra, 50 * MB);
        for _ in 0..6 {
            s.on_429();
        }
        s.cooldown_until = Instant::now() - Duration::from_secs(1);
        s.relax_ceiling();
        assert!(
            s.ceiling <= BandwidthTier::Ultra.max_conns() / 2,
            "多次 429 后天花板应长期压低, 实得 {}",
            s.ceiling
        );
    }
}
