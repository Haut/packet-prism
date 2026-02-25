use std::net::IpAddr;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[repr(i32)]
#[derive(Clone, Copy)]
enum SlotState {
    Idle = 0,
    Busy = 1,
    Cooling = 2,
}

// Align to 128 bytes (Apple Silicon cache-line size) to prevent false sharing
// between slots under concurrent access.
#[repr(C, align(128))]
pub struct Slot {
    state: AtomicI32,
    _pad0: [u8; 60],
    cool_until_ms: AtomicU64,
    _pad1: [u8; 56],
    pub ip: Option<IpAddr>,
    pub client: reqwest::Client,
}

impl Slot {
    fn new(ip: Option<IpAddr>) -> Self {
        Slot {
            state: AtomicI32::new(SlotState::Idle as i32),
            _pad0: [0u8; 60],
            cool_until_ms: AtomicU64::new(0),
            _pad1: [0u8; 56],
            ip,
            client: build_client(ip),
        }
    }
}

pub struct Pool {
    slots: Vec<Slot>,
    next: AtomicU64,
    epoch: Instant,
}

fn build_client(ip: Option<IpAddr>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(32)
        .tcp_keepalive(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .tcp_nodelay(true);

    if let Some(addr) = ip {
        builder = builder.local_address(addr);
    }

    builder.build().expect("failed to build reqwest client")
}

impl Pool {
    pub fn new(ips: &[IpAddr]) -> Self {
        let slots = if ips.is_empty() {
            vec![Slot::new(None)]
        } else {
            ips.iter().map(|&ip| Slot::new(Some(ip))).collect()
        };

        Pool {
            slots,
            next: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    pub fn acquire(&self) -> Option<&Slot> {
        let n = self.slots.len();
        let start = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % n;
        let now = self.now_ms();

        for i in 0..n {
            let slot = &self.slots[(start + i) % n];
            let state = slot.state.load(Ordering::Acquire);

            if state == SlotState::Cooling as i32 {
                if now >= slot.cool_until_ms.load(Ordering::Acquire)
                    && slot
                        .state
                        .compare_exchange(
                            SlotState::Cooling as i32,
                            SlotState::Busy as i32,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                {
                    return Some(slot);
                }
                continue;
            }

            if slot
                .state
                .compare_exchange(
                    SlotState::Idle as i32,
                    SlotState::Busy as i32,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(slot);
            }
        }

        None
    }

    pub fn release(&self, slot: &Slot) {
        slot.state.store(SlotState::Idle as i32, Ordering::Release);
    }

    pub fn cooldown(&self, slot: &Slot, duration: Duration) {
        let until = self.now_ms() + duration.as_millis() as u64;
        slot.cool_until_ms.store(until, Ordering::Relaxed);
        slot.state
            .store(SlotState::Cooling as i32, Ordering::Release);
    }

    #[allow(clippy::len_without_is_empty)] // Pool always has at least 1 slot
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[cfg(test)]
    fn slot(&self, index: usize) -> &Slot {
        &self.slots[index]
    }
}

#[cfg(test)]
impl Slot {
    fn state_raw(&self) -> i32 {
        self.state.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_pool(n: usize) -> Pool {
        let ips: Vec<IpAddr> = (1..=n)
            .map(|i| format!("10.0.0.{i}").parse().unwrap())
            .collect();
        Pool::new(&ips)
    }

    #[test]
    fn test_new_pool_default_single_slot() {
        let pool = Pool::new(&[]);
        assert_eq!(pool.len(), 1);
        assert!(pool.slot(0).ip.is_none());
        assert_eq!(pool.slot(0).state_raw(), SlotState::Idle as i32);
    }

    #[test]
    fn test_new_pool_multiple_ips() {
        let pool = test_pool(3);
        assert_eq!(pool.len(), 3);
        for i in 0..3 {
            let expected: IpAddr = format!("10.0.0.{}", i + 1).parse().unwrap();
            assert_eq!(pool.slot(i).ip, Some(expected));
            assert_eq!(pool.slot(i).state_raw(), SlotState::Idle as i32);
        }
    }

    #[test]
    fn test_acquire_idle_to_busy() {
        let pool = test_pool(1);
        let slot = pool.acquire().expect("should acquire");
        assert_eq!(slot.state_raw(), SlotState::Busy as i32);
    }

    #[test]
    fn test_release_busy_to_idle() {
        let pool = test_pool(1);
        let slot = pool.acquire().unwrap();
        assert_eq!(slot.state_raw(), SlotState::Busy as i32);
        pool.release(slot);
        assert_eq!(pool.slot(0).state_raw(), SlotState::Idle as i32);
    }

    #[test]
    fn test_cooldown_sets_cooling_state() {
        let pool = test_pool(1);
        let slot = pool.acquire().unwrap();
        pool.cooldown(slot, Duration::from_millis(100));
        assert_eq!(pool.slot(0).state_raw(), SlotState::Cooling as i32);
    }

    #[test]
    fn test_acquire_skips_unexpired_cooling() {
        let pool = test_pool(2);
        // Acquire and cooldown slot 0
        let slot0 = pool.acquire().unwrap();
        assert_eq!(slot0.ip, Some("10.0.0.1".parse().unwrap()));
        pool.cooldown(slot0, Duration::from_secs(10));

        // Next acquire should skip cooling slot and return slot 1
        let slot1 = pool.acquire().unwrap();
        assert_eq!(slot1.ip, Some("10.0.0.2".parse().unwrap()));
        pool.release(slot1);
    }

    #[tokio::test]
    async fn test_acquire_cooling_after_expiry() {
        let pool = test_pool(1);
        let slot = pool.acquire().unwrap();
        pool.cooldown(slot, Duration::from_millis(10));

        // Immediately should fail (still cooling)
        assert!(pool.acquire().is_none());

        // Wait for cooldown to expire
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Should now succeed (Cooling → Busy)
        let slot = pool.acquire().expect("should acquire after cooldown");
        assert_eq!(slot.state_raw(), SlotState::Busy as i32);
    }

    #[test]
    fn test_all_busy_returns_none() {
        let pool = test_pool(2);
        let _s0 = pool.acquire().unwrap();
        let _s1 = pool.acquire().unwrap();
        assert!(pool.acquire().is_none());
    }

    #[test]
    fn test_round_robin_distribution() {
        let pool = test_pool(3);
        let mut order = Vec::new();
        for _ in 0..9 {
            let slot = pool.acquire().unwrap();
            let last_octet: u8 = match slot.ip.unwrap() {
                IpAddr::V4(v4) => v4.octets()[3],
                _ => unreachable!(),
            };
            pool.release(slot);
            order.push(last_octet);
        }
        // Round-robin: 1,2,3,1,2,3,1,2,3
        assert_eq!(order, vec![1, 2, 3, 1, 2, 3, 1, 2, 3]);
    }

    #[tokio::test]
    async fn test_concurrent_acquire_release() {
        let pool = Arc::new(test_pool(4));
        let mut handles = Vec::new();
        for _ in 0..20 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..10 {
                    // Spin until we acquire (other tasks may hold all slots)
                    let slot = loop {
                        if let Some(s) = pool.acquire() {
                            break s;
                        }
                        tokio::task::yield_now().await;
                    };
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    pool.release(slot);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // All slots should be idle
        for i in 0..4 {
            assert_eq!(pool.slot(i).state_raw(), SlotState::Idle as i32);
        }
    }

    #[tokio::test]
    async fn test_concurrent_no_double_grant() {
        let pool = Arc::new(test_pool(1));
        let mut handles = Vec::new();
        for _ in 0..100 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move { pool.acquire().is_some() }));
        }
        let mut granted = 0;
        for h in handles {
            if h.await.unwrap() {
                granted += 1;
            }
        }
        assert_eq!(granted, 1, "exactly one task should acquire the slot");
    }
}
