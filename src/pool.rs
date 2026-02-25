use std::net::IpAddr;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const STATE_IDLE: i32 = 0;
const STATE_BUSY: i32 = 1;
const STATE_COOLING: i32 = 2;

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
        .redirect(reqwest::redirect::Policy::none());

    if let Some(addr) = ip {
        builder = builder.local_address(addr);
    }

    builder.build().expect("failed to build reqwest client")
}

impl Pool {
    pub fn new(ips: &[IpAddr]) -> Self {
        let slots = if ips.is_empty() {
            vec![Slot {
                state: AtomicI32::new(STATE_IDLE),
                _pad0: [0u8; 60],
                cool_until_ms: AtomicU64::new(0),
                _pad1: [0u8; 56],
                ip: None,
                client: build_client(None),
            }]
        } else {
            ips.iter()
                .map(|&ip| Slot {
                    state: AtomicI32::new(STATE_IDLE),
                    _pad0: [0u8; 60],
                    cool_until_ms: AtomicU64::new(0),
                    _pad1: [0u8; 56],
                    ip: Some(ip),
                    client: build_client(Some(ip)),
                })
                .collect()
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

            if state == STATE_COOLING {
                if now >= slot.cool_until_ms.load(Ordering::Acquire)
                    && slot
                        .state
                        .compare_exchange(
                            STATE_COOLING,
                            STATE_BUSY,
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
                .compare_exchange(STATE_IDLE, STATE_BUSY, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(slot);
            }
        }

        None
    }

    pub fn release(&self, slot: &Slot) {
        slot.state.store(STATE_IDLE, Ordering::Release);
    }

    pub fn cooldown(&self, slot: &Slot, duration: Duration) {
        let until = self.now_ms() + duration.as_millis() as u64;
        slot.cool_until_ms.store(until, Ordering::Release);
        slot.state.store(STATE_COOLING, Ordering::Release);
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
}
