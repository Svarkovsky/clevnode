use crate::constants::TICKET_LENGTH;
#[cfg(feature = "pow")]
use crate::{constants::*, msgpack};
#[cfg(feature = "pow")]
use alloc::{boxed::Box, vec::Vec};
#[cfg(feature = "pow")]
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use leviculum_core::crypto::truncated_hash;
#[cfg(feature = "pow")]
use leviculum_core::crypto::{derive_key, full_hash};
#[cfg(feature = "pow")]
use rand_core::CryptoRngCore;
#[cfg(feature = "pow")]
use sha2::{Digest, Sha256};

#[cfg(feature = "pow")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampError {
    InvalidCost,
    Cancelled,
}

#[cfg(feature = "pow")]
impl core::fmt::Display for StampError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidCost => write!(f, "stamp cost cannot be mined"),
            Self::Cancelled => write!(f, "stamp generation cancelled"),
        }
    }
}
#[cfg(feature = "pow")]
pub trait Yield {
    type Fut<'a>: Future<Output = ()> + Send + 'a
    where
        Self: 'a;
    fn yield_now(&mut self) -> Self::Fut<'_>;
}
#[cfg(feature = "pow")]
pub struct ReadyYield;
#[cfg(feature = "pow")]
impl Yield for ReadyYield {
    type Fut<'a> = core::future::Ready<()>;
    fn yield_now(&mut self) -> Self::Fut<'_> {
        core::future::ready(())
    }
}

/// Runtime-independent cooperative yield. The future returns `Pending` once,
/// wakes its task, and completes on the next poll.
#[cfg(feature = "pow")]
#[derive(Default)]
pub struct CooperativeYield;
#[cfg(feature = "pow")]
pub struct YieldOnce(bool);
#[cfg(feature = "pow")]
impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}
#[cfg(feature = "pow")]
impl Yield for CooperativeYield {
    type Fut<'a> = YieldOnce;
    fn yield_now(&mut self) -> Self::Fut<'_> {
        YieldOnce(false)
    }
}

/// Override point for a threaded (for example Rayon) implementation.
#[cfg(feature = "pow")]
pub trait StampExecutor {
    /// Mine a PoW stamp. The future is `Send` so a host can hand a
    /// peer-priced grind to a work-stealing runtime instead of pinning it to
    /// the thread that started it; that is also why [`Yield::Fut`] is `Send`.
    fn generate<'a>(
        &'a mut self,
        material: &'a [u8],
        cost: u8,
        rounds: usize,
    ) -> Pin<Box<dyn Future<Output = Result<[u8; 32], StampError>> + Send + 'a>>;

    /// Validate a PoW stamp without blocking the protocol event loop. Custom
    /// executors can move this work to Rayon or hardware; the default streams
    /// the workblock and cooperatively yields.
    fn validate<'a>(
        &'a mut self,
        material: &'a [u8],
        stamp: &'a [u8; 32],
        cost: u8,
        rounds: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Option<u16>, StampError>> + 'a>>;
}

#[cfg(feature = "pow")]
pub struct CooperativeStamper<R, Y> {
    pub rng: R,
    pub scheduler: Y,
    pub yield_every: usize,
}
#[cfg(feature = "pow")]
impl<R: CryptoRngCore, Y: Yield> CooperativeStamper<R, Y> {
    pub fn new(rng: R, scheduler: Y) -> Self {
        Self {
            rng,
            scheduler,
            yield_every: 64,
        }
    }
    pub async fn workblock(&mut self, material: &[u8], rounds: usize) -> Vec<u8> {
        let mut w = Vec::with_capacity(rounds.saturating_mul(256));
        for n in 0..rounds {
            let mut encoded = Vec::new();
            msgpack::uint(&mut encoded, n as u64);
            let mut salt_input = Vec::with_capacity(material.len() + encoded.len());
            salt_input.extend_from_slice(material);
            salt_input.extend_from_slice(&encoded);
            let salt = full_hash(&salt_input);
            let start = w.len();
            w.resize(start + 256, 0);
            derive_key(material, Some(&salt), None, &mut w[start..]);
            if (n + 1).is_multiple_of(self.yield_every.max(1)) {
                self.scheduler.yield_now().await
            }
        }
        w
    }

    /// Build the SHA-256 state for `workblock` without retaining the workblock.
    /// A production delivery stamp therefore uses constant workspace (one
    /// 256-byte HKDF block) instead of the Python implementation's 768 KB.
    async fn workblock_hasher(&mut self, material: &[u8], rounds: usize) -> Sha256 {
        let mut hasher = Sha256::new();
        let mut block = [0u8; 256];
        for n in 0..rounds {
            let mut encoded = Vec::new();
            msgpack::uint(&mut encoded, n as u64);
            let mut salt_input = Vec::with_capacity(material.len() + encoded.len());
            salt_input.extend_from_slice(material);
            salt_input.extend_from_slice(&encoded);
            let salt = full_hash(&salt_input);
            derive_key(material, Some(&salt), None, &mut block);
            hasher.update(block);
            if (n + 1).is_multiple_of(self.yield_every.max(1)) {
                self.scheduler.yield_now().await;
            }
        }
        hasher
    }
    pub async fn generate(
        &mut self,
        material: &[u8],
        cost: u8,
        rounds: usize,
    ) -> Result<[u8; 32], StampError> {
        if cost == 0 {
            let mut stamp = [0u8; 32];
            self.rng.fill_bytes(&mut stamp);
            return Ok(stamp);
        }
        // At cost 255 the reference's target is `1 << 1` (LXStamper.py:74), so
        // the search below needs a 256-bit digest of 0, 1 or 2 and never
        // returns. The reference never announces such a cost
        // (LXMRouter.py:1042-1045), so refusing it here cannot affect a
        // conforming peer; see `LxmfRouter::outbound_stamp_cost`, which is the
        // primary guard. Validation is unaffected: checking a stamp at any cost
        // is a single hash.
        if cost == 255 {
            return Err(StampError::InvalidCost);
        }
        let base = self.workblock_hasher(material, rounds).await;
        let mut tries = 0usize;
        loop {
            let mut s = [0; 32];
            self.rng.fill_bytes(&mut s);
            let candidate = digest_from_base(&base, &s);
            if digest_valid(&candidate, cost) {
                return Ok(s);
            }
            tries += 1;
            if tries.is_multiple_of(self.yield_every.max(1)) {
                self.scheduler.yield_now().await
            }
        }
    }

    pub async fn validate_stamp(
        &mut self,
        material: &[u8],
        stamp: &[u8; 32],
        cost: u8,
        rounds: usize,
    ) -> Result<Option<u16>, StampError> {
        if cost == 0 {
            return Ok(Some(0));
        }
        let base = self.workblock_hasher(material, rounds).await;
        let digest = digest_from_base(&base, stamp);
        Ok(digest_valid(&digest, cost).then(|| digest_value(&digest)))
    }
}
#[cfg(feature = "pow")]
impl<R: CryptoRngCore> CooperativeStamper<R, CooperativeYield> {
    /// Construct the default, genuinely cooperative single-threaded engine.
    pub fn cooperative(rng: R) -> Self {
        Self::new(rng, CooperativeYield)
    }
}
#[cfg(feature = "pow")]
impl<R: CryptoRngCore + Send, Y: Yield + Send> StampExecutor for CooperativeStamper<R, Y> {
    fn generate<'a>(
        &'a mut self,
        m: &'a [u8],
        c: u8,
        r: usize,
    ) -> Pin<Box<dyn Future<Output = Result<[u8; 32], StampError>> + Send + 'a>> {
        Box::pin(self.generate(m, c, r))
    }

    fn validate<'a>(
        &'a mut self,
        material: &'a [u8],
        stamp: &'a [u8; 32],
        cost: u8,
        rounds: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Option<u16>, StampError>> + 'a>> {
        Box::pin(self.validate_stamp(material, stamp, cost, rounds))
    }
}
#[cfg(feature = "pow")]
pub fn digest(workblock: &[u8], stamp: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(workblock);
    h.update(stamp);
    h.finalize().into()
}
#[cfg(feature = "pow")]
pub fn value(workblock: &[u8], stamp: &[u8; 32]) -> u16 {
    digest_value(&digest(workblock, stamp))
}
#[cfg(feature = "pow")]
fn digest_value(digest: &[u8; 32]) -> u16 {
    digest
        .iter()
        .fold((0u16, true), |(n, go), b| {
            if go {
                let z = b.leading_zeros() as u16;
                (n + z, z == 8)
            } else {
                (n, false)
            }
        })
        .0
}
#[cfg(feature = "pow")]
pub fn valid(workblock: &[u8], stamp: &[u8; 32], cost: u8) -> bool {
    digest_valid(&digest(workblock, stamp), cost)
}
#[cfg(feature = "pow")]
fn digest_valid(hash: &[u8; 32], cost: u8) -> bool {
    if cost == 0 {
        return true;
    }
    let mut target = [0u8; 32];
    let bit = 256usize - cost as usize;
    target[31 - bit / 8] = 1 << (bit % 8);
    *hash <= target
}
#[cfg(feature = "pow")]
fn digest_from_base(base: &Sha256, stamp: &[u8; 32]) -> [u8; 32] {
    let mut hasher = base.clone();
    hasher.update(stamp);
    hasher.finalize().into()
}
pub fn ticket_stamp(ticket: &[u8; TICKET_LENGTH], message_id: &[u8; 32]) -> [u8; 16] {
    let mut d = [0u8; TICKET_LENGTH + 32];
    d[..TICKET_LENGTH].copy_from_slice(ticket);
    d[TICKET_LENGTH..].copy_from_slice(message_id);
    truncated_hash(&d)
}
#[cfg(feature = "pow")]
pub async fn validate<R: CryptoRngCore, Y: Yield>(
    engine: &mut CooperativeStamper<R, Y>,
    material: &[u8; 32],
    stamp: &[u8],
    cost: u8,
    rounds: usize,
    tickets: &[[u8; 16]],
) -> Result<Option<u16>, StampError> {
    for t in tickets {
        if stamp == ticket_stamp(t, material) {
            return Ok(Some(COST_TICKET));
        }
    }
    let Ok(stamp) = <&[u8; 32]>::try_from(stamp) else {
        return Ok(None);
    };
    let base = engine.workblock_hasher(material, rounds).await;
    let digest = digest_from_base(&base, stamp);
    Ok(digest_valid(&digest, cost).then(|| digest_value(&digest)))
}

#[cfg(test)]
mod tests {
    use super::ticket_stamp;

    #[test]
    fn ticket_stamp_matches_python_golden_vector() {
        let mut ticket = [0u8; 16];
        for (value, byte) in ticket.iter_mut().enumerate() {
            *byte = value as u8;
        }
        let mut message_id = [0u8; 32];
        for (value, byte) in message_id.iter_mut().enumerate() {
            *byte = value as u8;
        }

        assert_eq!(
            ticket_stamp(&ticket, &message_id),
            [
                0x95, 0xe6, 0x6c, 0xe4, 0x08, 0xbc, 0xb4, 0x51, 0x34, 0xe0, 0x8d, 0x15, 0x9c, 0x51,
                0xe1, 0xf4,
            ]
        );
    }

    /// Generation refuses the one cost whose search cannot terminate, while
    /// validation at that cost stays available (Codeberg #181).
    ///
    /// `stamp_valid` (LXStamper.py:73-77) computes `target = 1 << 256-cost`, so
    /// at 255 a valid stamp needs a digest of 0, 1 or 2. The reference never
    /// announces that cost (LXMRouter.py:1042-1045), so refusing it costs no
    /// conforming peer anything. 254 is inside the announceable window and must
    /// still be attempted, which is what keeps this from becoming an arbitrary
    /// ceiling.
    ///
    /// What this test cannot catch: it does not show that 254 is attempted,
    /// because that search does not finish. The legal side of the boundary is
    /// pinned instead by `router::persistence_tests::
    /// hostile_announced_stamp_cost_is_not_mined`, which asserts that
    /// `outbound_stamp_cost` still yields `Some(254)`.
    #[cfg(feature = "pow")]
    #[test]
    fn generation_refuses_the_non_terminating_cost() {
        use super::{CooperativeStamper, StampError};
        use rand_core::OsRng;

        let mut stamper = CooperativeStamper::cooperative(OsRng);
        assert_eq!(
            futures::executor::block_on(stamper.generate(b"material", 255, 1)),
            Err(StampError::InvalidCost)
        );
        // Validation is a single hash at any cost and stays available, so an
        // inbound stamp claimed at 255 is still checked (and rejected) rather
        // than erroring out.
        assert_eq!(
            futures::executor::block_on(stamper.validate_stamp(b"material", &[0; 32], 255, 1)),
            Ok(None)
        );
    }
}
