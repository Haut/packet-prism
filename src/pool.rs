use std::net::IpAddr;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STATE_IDLE: i32 = 0;
const STATE_BUSY: i32 = 1;
const STATE_COOLING: i32 = 2;

pub struct Slot {
    pub ip: Option<IpAddr>,
    pub client: reqwest::Client,
    state: AtomicI32,
    cool_until: AtomicI64,
}

pub struct Pool {
    slots: Vec<Slot>,
    next: AtomicU64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn build_client(ip: Option<IpAddr>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .redirect(reqwest::redirect::Policy::none());

    if let Some(addr) = ip {
        builder = builder.local_address(addr);
    }

    builder.build().expect("failed to build reqwest client")
}

impl Pool {
    pub fn new(ips: &[IpAddr]) -> Self {
        let slots = if ips.is_empty() {
            // No IPs specified — single slot using OS default
            vec![Slot {
                ip: None,
                client: build_client(None),
                state: AtomicI32::new(STATE_IDLE),
                cool_until: AtomicI64::new(0),
            }]
        } else {
            ips.iter()
                .map(|&ip| Slot {
                    ip: Some(ip),
                    client: build_client(Some(ip)),
                    state: AtomicI32::new(STATE_IDLE),
                    cool_until: AtomicI64::new(0),
                })
                .collect()
        };

        Pool {
            slots,
            next: AtomicU64::new(0),
        }
    }

    pub fn acquire(&self) -> Option<&Slot> {
        let n = self.slots.len();
        let start = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % n;
        let now = now_unix();

        for i in 0..n {
            let slot = &self.slots[(start + i) % n];
            let state = slot.state.load(Ordering::Acquire);

            if state == STATE_COOLING {
                if now >= slot.cool_until.load(Ordering::Acquire)
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
        let until = now_unix() + duration.as_secs() as i64;
        slot.cool_until.store(until, Ordering::Release);
        slot.state.store(STATE_COOLING, Ordering::Release);
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
}
