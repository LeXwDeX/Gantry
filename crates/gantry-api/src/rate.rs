//! 限流（api.md §3.5、§3.7，本文 §7.11）。
//!
//! 内存令牌桶，键为 `(主体, 类)`：主体是设备（配对端点为源 IP）。容量 = 每分钟
//! 额度，匀速补充；空闲桶 15 分钟后淘汰；进程重启后重置（可接受）。时间取注入的
//! `Clock`，测试用 `FakeClock` 推进。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gantry_core::ids::Clock;

use crate::consts::{
    RATE_IDLE_EVICT, RATE_MESSAGE_PER_MIN, RATE_PAIR_PER_MIN, RATE_READ_PER_MIN, RATE_SIGN_PER_MIN,
    RATE_WRITE_PER_MIN,
};
use crate::registry::RateClass;

const WINDOW_MS: f64 = 60_000.0;

pub fn per_minute(class: RateClass) -> u32 {
    match class {
        RateClass::Read => RATE_READ_PER_MIN,
        RateClass::Write => RATE_WRITE_PER_MIN,
        RateClass::Message => RATE_MESSAGE_PER_MIN,
        RateClass::Sign => RATE_SIGN_PER_MIN,
        RateClass::PairIp => RATE_PAIR_PER_MIN,
    }
}

/// `RateLimit-*` 头的取值；超限时 `retry_after` 有值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateState {
    pub limit: u32,
    pub remaining: u32,
    /// 桶补满所需秒数（向上取整）。
    pub reset_secs: u64,
    /// 超限时：下一个令牌可用前的秒数（向上取整，至少 1）。
    pub retry_after: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    at_ms: i64,
}

pub struct RateLimiter {
    clock: Arc<dyn Clock>,
    buckets: Mutex<HashMap<(String, RateClass), Bucket>>,
}

impl RateLimiter {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// 消耗一个令牌。
    pub fn take(&self, subject: &str, class: RateClass) -> RateState {
        self.step(subject, class, true)
    }

    /// 只读当前状态，不扣（步骤 6 之前的错误响应用，本文 §7.1 步骤 14）。
    pub fn peek(&self, subject: &str, class: RateClass) -> RateState {
        self.step(subject, class, false)
    }

    fn step(&self, subject: &str, class: RateClass, consume: bool) -> RateState {
        let now = self.clock.now().as_millis();
        let limit = per_minute(class);
        let capacity = f64::from(limit);
        let rate_per_ms = capacity / WINDOW_MS;
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        evict_idle(&mut buckets, now);
        let key = (subject.to_owned(), class);
        let mut bucket = buckets.get(&key).copied().unwrap_or(Bucket {
            tokens: capacity,
            at_ms: now,
        });
        let elapsed = (now - bucket.at_ms).max(0) as f64;
        bucket.tokens = (bucket.tokens + elapsed * rate_per_ms).min(capacity);
        bucket.at_ms = now;
        let mut retry_after = None;
        if consume {
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
            } else {
                let wait_ms = (1.0 - bucket.tokens) / rate_per_ms;
                retry_after = Some(((wait_ms / 1000.0).ceil() as u64).max(1));
            }
            buckets.insert(key, bucket);
        }
        let missing = capacity - bucket.tokens;
        RateState {
            limit,
            remaining: bucket.tokens.floor() as u32,
            reset_secs: (missing / rate_per_ms / 1000.0).ceil() as u64,
            retry_after,
        }
    }
}

fn evict_idle(buckets: &mut HashMap<(String, RateClass), Bucket>, now: i64) {
    let idle = RATE_IDLE_EVICT.as_millis() as i64;
    buckets.retain(|_, b| now - b.at_ms < idle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gantry_core::testing::FakeClock;

    #[test]
    fn exhausts_and_refills() {
        let clock = Arc::new(FakeClock::new(1_000_000));
        let limiter = RateLimiter::new(clock.clone());
        for _ in 0..30 {
            assert!(limiter
                .take("dev", RateClass::Message)
                .retry_after
                .is_none());
        }
        let blocked = limiter.take("dev", RateClass::Message);
        assert_eq!(blocked.remaining, 0);
        assert_eq!(blocked.retry_after, Some(2));
        // 不同主体、不同类互不影响
        assert!(limiter
            .take("other", RateClass::Message)
            .retry_after
            .is_none());
        assert!(limiter.take("dev", RateClass::Read).retry_after.is_none());
        clock.advance_secs(2);
        assert!(limiter
            .take("dev", RateClass::Message)
            .retry_after
            .is_none());
    }

    #[test]
    fn peek_does_not_consume() {
        let clock = Arc::new(FakeClock::new(0));
        let limiter = RateLimiter::new(clock);
        for _ in 0..5 {
            assert_eq!(limiter.peek("dev", RateClass::Read).remaining, 600);
        }
    }
}
