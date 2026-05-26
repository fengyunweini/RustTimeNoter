//! 段聚合状态机。**不依赖 Windows API**，便于单元测试。
//!
//! 输入事件流（按时间到达）：
//! - `Foreground { app, t }`        前台切换
//! - `IdleTick { now, last_input }`  AFK 探测
//! - `SessionLock { t }` / `SessionUnlock { t }`
//! - `Suspend { t }` / `Resume { t }`
//! - `Shutdown { t }`
//!
//! 输出：完整的 `Segment`，由调用方塞给 writer。
//!
//! 语义：
//! - 锁屏 / 休眠 / AFK 期间不计时。
//! - AFK 切割边界 = `last_input + afk_threshold`。
//! - 同一 app 连续切换（标题不变）会合并？v1 不合并：每次 Foreground 即新段，避免逻辑复杂。
//!   *但*：相同 app+title 的连续 Foreground（罕见，常见于 Alt-Tab 来回）确实会产生琐碎段；
//!   留待 query 层合并。

use crate::storage::Segment;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppKey {
    pub path: String,
    pub basename: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// 前台切换。
    /// `t` = 切换发生时刻；`last_input` = OS 报告的最近一次键鼠输入时刻
    /// （用 `GetLastInputInfo` 取，绝不能用 `t` 充数 —— 否则 AFK 期间任何
    /// 抢焦窗口都会把 AFK 计时器重置）。
    Foreground { app: AppKey, t: u64, last_input: u64 },
    IdleTick { now: u64, last_input: u64 },
    SessionLock { t: u64 },
    SessionUnlock { t: u64 },
    Suspend { t: u64 },
    Resume { t: u64 },
    Shutdown { t: u64 },
}

#[derive(Debug, Clone)]
struct Active {
    app: AppKey,
    started_at: u64,
    /// 最近一次"已确认有键鼠输入"的时刻；用于 AFK 切割。
    last_input: u64,
}

#[derive(Debug)]
pub struct Aggregator {
    afk_threshold: u64,
    active: Option<Active>,
    /// 是否被外部门控关闭（锁屏 / 休眠）。被关闭时不允许有 active。
    suppressed: bool,
}

impl Aggregator {
    pub fn new(afk_threshold_secs: u64) -> Self {
        Self { afk_threshold: afk_threshold_secs.max(1), active: None, suppressed: false }
    }

    /// 处理一条事件；返回此次产生的所有 segment（通常是 0 或 1）。
    pub fn handle(&mut self, ev: Event) -> Vec<Segment> {
        let mut out = Vec::new();
        match ev {
            Event::Foreground { app, t, last_input } => {
                if self.suppressed {
                    return out; // 锁屏/休眠期间忽略前台切换
                }
                // 用 OS 报告的 last_input 作为种子；夹到 [0, t] 之间。
                let seeded_last_input = last_input.min(t);
                let idle_for = t.saturating_sub(seeded_last_input);
                if idle_for >= self.afk_threshold {
                    // 用户根本没操作，但窗口抢了焦点 —— 不能算作活跃。
                    // 把旧段（若有）按 AFK 边界裁掉，不开新段。
                    if let Some(a) = &self.active {
                        let cut_at = (a.last_input.max(a.started_at) + self.afk_threshold).min(t);
                        if let Some(seg) = self.close_active(cut_at) {
                            out.push(seg);
                        }
                    }
                    return out;
                }
                if let Some(seg) = self.close_active(t) {
                    out.push(seg);
                }
                self.active = Some(Active { app, started_at: t, last_input: seeded_last_input });
            }
            Event::IdleTick { now, last_input } => {
                if let Some(a) = &mut self.active {
                    // 信任 OS：直接覆盖，不要 max。max 会让被抢焦点污染过的
                    // last_input 卡在未来时刻，永远检测不到 AFK。
                    // 夹到 [started_at, now]：不可能比段开始更早，也不可能比现在更晚。
                    a.last_input = last_input.clamp(a.started_at, now);
                    let idle_for = now.saturating_sub(a.last_input);
                    if idle_for >= self.afk_threshold {
                        let cut_at = (a.last_input + self.afk_threshold).min(now);
                        if let Some(seg) = self.close_active(cut_at) {
                            out.push(seg);
                        }
                    }
                }
            }
            Event::SessionLock { t } | Event::Suspend { t } => {
                if let Some(seg) = self.close_active(t) {
                    out.push(seg);
                }
                self.suppressed = true;
            }
            Event::SessionUnlock { t } | Event::Resume { t } => {
                self.suppressed = false;
                // 不在此处自动开始：等待下一个 Foreground 事件。
                let _ = t;
            }
            Event::Shutdown { t } => {
                if let Some(seg) = self.close_active(t) {
                    out.push(seg);
                }
            }
        }
        out
    }

    /// 当前是否有活跃段。runtime 在 IdleTick 发现用户回来但 agg 已空闲时，
    /// 用它判断要不要补一个 Foreground 事件以恢复跟踪。
    pub fn is_active(&self) -> bool {
        self.active.is_some() && !self.suppressed
    }

    pub fn is_suppressed(&self) -> bool {
        self.suppressed
    }

    fn close_active(&mut self, end_t: u64) -> Option<Segment> {
        let a = self.active.take()?;
        let end = end_t.max(a.started_at);
        if end <= a.started_at {
            return None;
        }
        Some(Segment {
            app_path: a.app.path,
            app_basename: a.app.basename,
            title: a.app.title,
            start_unix: a.started_at,
            end_unix: end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> AppKey {
        AppKey { path: format!("C:/{}.exe", name), basename: format!("{name}.exe"), title: None }
    }

    fn fg(name: &str, t: u64) -> Event {
        Event::Foreground { app: app(name), t, last_input: t }
    }

    #[test]
    fn switch_emits_segment() {
        let mut a = Aggregator::new(300);
        assert!(a.handle(fg("a", 100)).is_empty());
        let segs = a.handle(fg("b", 150));
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].duration(), 50);
        assert_eq!(segs[0].app_basename, "a.exe");
    }

    #[test]
    fn afk_cuts_segment() {
        let mut a = Aggregator::new(300);
        a.handle(fg("a", 0));
        // 用户在 t=10 有输入；t=400 探测，距 last_input 390 > 300
        let segs = a.handle(Event::IdleTick { now: 400, last_input: 10 });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].end_unix, 10 + 300);
    }

    #[test]
    fn lock_suppresses_then_unlock_resumes_on_foreground() {
        let mut a = Aggregator::new(300);
        a.handle(fg("a", 0));
        let segs = a.handle(Event::SessionLock { t: 50 });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].duration(), 50);
        // 锁屏期间切窗忽略
        assert!(a.handle(fg("b", 60)).is_empty());
        // 解锁后等下一次 Foreground
        a.handle(Event::SessionUnlock { t: 100 });
        assert!(a.handle(Event::IdleTick { now: 110, last_input: 100 }).is_empty());
        a.handle(fg("c", 120));
        let segs = a.handle(Event::Shutdown { t: 130 });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].app_basename, "c.exe");
    }

    #[test]
    fn suspend_resume_keeps_no_segment_until_foreground() {
        let mut a = Aggregator::new(300);
        a.handle(fg("a", 0));
        a.handle(Event::Suspend { t: 30 });
        a.handle(Event::Resume { t: 1000 });
        let segs = a.handle(Event::Shutdown { t: 1010 });
        assert!(segs.is_empty()); // resume 后无新 foreground，无段
    }

    #[test]
    fn idle_tick_with_recent_input_no_cut() {
        let mut a = Aggregator::new(300);
        a.handle(fg("a", 0));
        let segs = a.handle(Event::IdleTick { now: 100, last_input: 90 });
        assert!(segs.is_empty());
    }

    // ── 回归用例：AFK 期间被抢焦点不能重置 AFK 计时器 ──

    #[test]
    fn focus_steal_during_afk_does_not_reset_idle_clock() {
        let mut a = Aggregator::new(300);
        // t=0 用户打开 app a，之后离开
        a.handle(Event::Foreground { app: app("a"), t: 0, last_input: 0 });
        // t=100..400 用户 AFK；t=400 一个通知 popup 抢了焦点
        // last_input 在 OS 看来还是 0（用户根本没动过）
        let segs = a.handle(Event::Foreground { app: app("notif"), t: 400, last_input: 0 });
        // 旧段 a 应该袖珍 —— 但由于用户已 AFK，notif 也不能被认为活跃
        // 旧段裁到 0 + 300 = 300
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].app_basename, "a.exe");
        assert_eq!(segs[0].end_unix, 300);
        // notif 无活跃段；后续 IdleTick 不产生任何东西
        assert!(!a.is_active());
        let segs = a.handle(Event::IdleTick { now: 1000, last_input: 0 });
        assert!(segs.is_empty());
    }

    #[test]
    fn idle_tick_trusts_os_even_if_previous_value_was_higher() {
        let mut a = Aggregator::new(300);
        // Foreground 带了个可信的 last_input=100
        a.handle(Event::Foreground { app: app("a"), t: 100, last_input: 100 });
        // 紧跟一个补手 IdleTick，“伪造”高 last_input（现实中不可能发生，
        // 但以防 OS、时钟抬起等场景）——之后 OS 报告 low
        a.handle(Event::IdleTick { now: 200, last_input: 200 });
        // 现在 OS 说：last_input 还是 100（用户这期间根本没动）
        // 老版本用 max() 会卡在 200；新版本应该覆盖为 100。
        let segs = a.handle(Event::IdleTick { now: 500, last_input: 100 });
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].end_unix, 100 + 300);
    }
}
