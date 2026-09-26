//! LXMF stamp-ticket lifecycle.

use alloc::{collections::BTreeMap, vec::Vec};
use rand_core::CryptoRngCore;

use crate::{
    constants::{TICKET_EXPIRY, TICKET_GRACE, TICKET_INTERVAL, TICKET_RENEW},
    msgpack,
};

pub type Destination = [u8; 16];

#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub expires_unix: f64,
    pub secret: [u8; 16],
}

impl Ticket {
    /// Encode the value placed in `FIELD_TICKET`: `[expires, ticket]`.
    pub fn field_value(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(28);
        msgpack::array(&mut out, 2);
        msgpack::f64(&mut out, self.expires_unix);
        msgpack::bin(&mut out, &self.secret);
        out
    }

    pub fn from_field_value(data: &[u8]) -> Result<Self, msgpack::Error> {
        let mut pos = 0;
        if msgpack::array_len(data, &mut pos)? != 2 {
            return Err(msgpack::Error::Type);
        }
        let expires_unix = msgpack::read_number_f64(data, &mut pos)?;
        if !expires_unix.is_finite() {
            return Err(msgpack::Error::Type);
        }
        let secret: [u8; 16] = msgpack::read_bin(data, &mut pos)?
            .try_into()
            .map_err(|_| msgpack::Error::Type)?;
        if pos != data.len() {
            return Err(msgpack::Error::Trailing);
        }
        Ok(Self {
            expires_unix,
            secret,
        })
    }
}

/// Persistent ticket data. The router snapshot serialises this structure.
#[derive(Debug, Clone, Default)]
pub struct TicketStore {
    inbound: BTreeMap<Destination, Vec<Ticket>>,
    outbound: BTreeMap<Destination, Ticket>,
    last_deliveries: BTreeMap<Destination, f64>,
}

impl TicketStore {
    /// Return the total number of persisted ticket-related records.
    ///
    /// This includes issued tickets, received tickets and delivery-throttle
    /// timestamps, and can be used to enforce a bound before inserting a new
    /// issued ticket.
    pub fn entry_count(&self) -> usize {
        self.inbound
            .values()
            .fold(0usize, |total, entries| total.saturating_add(entries.len()))
            .saturating_add(self.outbound.len())
            .saturating_add(self.last_deliveries.len())
    }

    /// Find an issued ticket that Python LXMF's renewal policy would reuse.
    ///
    /// This only checks the ticket's remaining lifetime. [`Self::issue`] also
    /// applies the delivery interval before consulting this lookup.
    pub fn reusable_issued(&self, destination: &Destination, now_unix: f64) -> Option<&Ticket> {
        if !now_unix.is_finite() {
            return None;
        }
        self.inbound.get(destination).and_then(|entries| {
            entries.iter().find(|entry| {
                entry.expires_unix.is_finite()
                    && entry.expires_unix - now_unix > TICKET_RENEW as f64
            })
        })
    }

    pub fn issue<R: CryptoRngCore>(
        &mut self,
        destination: Destination,
        now_unix: f64,
        rng: &mut R,
    ) -> Option<Ticket> {
        if !now_unix.is_finite() {
            return None;
        }
        if self
            .last_deliveries
            .get(&destination)
            .is_some_and(|last| now_unix - *last < TICKET_INTERVAL as f64)
        {
            return None;
        }

        if let Some(existing) = self.reusable_issued(&destination, now_unix) {
            return Some(existing.clone());
        }

        let mut secret = [0u8; 16];
        rng.fill_bytes(&mut secret);
        let ticket = Ticket {
            expires_unix: now_unix + TICKET_EXPIRY as f64,
            secret,
        };
        self.inbound
            .entry(destination)
            .or_default()
            .push(ticket.clone());
        Some(ticket)
    }

    pub fn mark_delivered(&mut self, destination: Destination, now_unix: f64) {
        self.last_deliveries.insert(destination, now_unix);
    }

    /// Remember a ticket received from a verified message.
    pub fn remember(&mut self, source: Destination, ticket: Ticket, now_unix: f64) -> bool {
        if !ticket.expires_unix.is_finite()
            || !now_unix.is_finite()
            || ticket.expires_unix <= now_unix
        {
            return false;
        }
        self.outbound.insert(source, ticket);
        true
    }

    /// Return whether the exact ticket was issued for `destination`.
    ///
    /// Expired tickets remain owned during their grace period, so this lookup
    /// deliberately does not apply a time filter.
    pub fn contains_inbound(&self, destination: &Destination, ticket: &Ticket) -> bool {
        self.inbound
            .get(destination)
            .is_some_and(|entries| entries.contains(ticket))
    }

    /// Validate a ticket stamp against unexpired tickets issued to `source`.
    pub fn validates_inbound_stamp(
        &self,
        source: &Destination,
        message_id: &[u8; 32],
        stamp: &[u8],
        now_unix: f64,
    ) -> bool {
        stamp.len() == 16
            && now_unix.is_finite()
            && self
                .inbound
                .get(source)
                .into_iter()
                .flatten()
                .filter(|entry| entry.expires_unix.is_finite() && entry.expires_unix > now_unix)
                .any(|entry| crate::stamp::ticket_stamp(&entry.secret, message_id) == stamp)
    }

    pub fn outbound(&self, destination: &Destination, now_unix: f64) -> Option<&Ticket> {
        self.outbound
            .get(destination)
            .filter(|entry| entry.expires_unix > now_unix)
    }

    pub fn inbound_secrets(&self, source: &Destination, now_unix: f64) -> Vec<[u8; 16]> {
        self.inbound
            .get(source)
            .into_iter()
            .flatten()
            .filter(|entry| entry.expires_unix > now_unix)
            .map(|entry| entry.secret)
            .collect()
    }

    /// Remove expired ticket material and report whether the persistent store changed.
    pub fn clean(&mut self, now_unix: f64) -> bool {
        let outbound_before = self.outbound.len();
        let inbound_destinations_before = self.inbound.len();
        let inbound_tickets_before: usize = self.inbound.values().map(Vec::len).sum();
        self.outbound
            .retain(|_, entry| entry.expires_unix > now_unix);
        self.inbound.retain(|_, entries| {
            entries.retain(|entry| entry.expires_unix + TICKET_GRACE as f64 > now_unix);
            !entries.is_empty()
        });
        outbound_before != self.outbound.len()
            || inbound_destinations_before != self.inbound.len()
            || inbound_tickets_before != self.inbound.values().map(Vec::len).sum()
    }

    pub(crate) fn inbound(&self) -> &BTreeMap<Destination, Vec<Ticket>> {
        &self.inbound
    }
    pub(crate) fn outbound_entries(&self) -> &BTreeMap<Destination, Ticket> {
        &self.outbound
    }
    pub(crate) fn last_deliveries(&self) -> &BTreeMap<Destination, f64> {
        &self.last_deliveries
    }
    pub(crate) fn restore_inbound(&mut self, destination: Destination, ticket: Ticket) {
        self.inbound.entry(destination).or_default().push(ticket);
    }
    pub(crate) fn restore_outbound(&mut self, destination: Destination, ticket: Ticket) {
        self.outbound.insert(destination, ticket);
    }
    pub(crate) fn restore_last_delivery(&mut self, destination: Destination, time: f64) {
        self.last_deliveries.insert(destination, time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    #[test]
    fn ticket_renewal_interval_and_grace_match_reference() {
        let mut store = TicketStore::default();
        let destination = [7; 16];
        let first = store.issue(destination, 1_000.0, &mut OsRng).unwrap();
        assert_eq!(
            Ticket::from_field_value(&first.field_value()).unwrap(),
            first
        );
        store.mark_delivered(destination, 1_000.0);
        assert!(store.issue(destination, 2_000.0, &mut OsRng).is_none());
        store.clean(first.expires_unix + TICKET_GRACE as f64 + 1.0);
        assert!(store.inbound_secrets(&destination, 0.0).is_empty());
    }

    #[test]
    fn ticket_field_rejects_non_finite_expiry() {
        for expires_unix in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let encoded = Ticket {
                expires_unix,
                secret: [1; 16],
            }
            .field_value();
            assert_eq!(
                Ticket::from_field_value(&encoded),
                Err(msgpack::Error::Type)
            );
        }
    }

    #[test]
    fn remember_requires_finite_future_expiry() {
        let mut store = TicketStore::default();
        let source = [2; 16];
        for expires_unix in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 100.0] {
            assert!(!store.remember(
                source,
                Ticket {
                    expires_unix,
                    secret: [3; 16],
                },
                100.0,
            ));
        }
        assert!(!store.remember(
            source,
            Ticket {
                expires_unix: 101.0,
                secret: [3; 16],
            },
            f64::NAN,
        ));
        assert_eq!(store.entry_count(), 0);

        assert!(store.remember(
            source,
            Ticket {
                expires_unix: 100.000_001,
                secret: [3; 16],
            },
            100.0,
        ));
        assert_eq!(store.entry_count(), 1);
    }

    #[test]
    fn entry_count_includes_every_persisted_ticket_record() {
        let mut store = TicketStore::default();
        let destination = [3; 16];
        store.restore_inbound(
            destination,
            Ticket {
                expires_unix: 200.0,
                secret: [1; 16],
            },
        );
        store.restore_inbound(
            destination,
            Ticket {
                expires_unix: 300.0,
                secret: [2; 16],
            },
        );
        store.restore_outbound(
            destination,
            Ticket {
                expires_unix: 400.0,
                secret: [3; 16],
            },
        );
        store.restore_last_delivery(destination, 100.0);

        assert_eq!(store.entry_count(), 4);
    }

    #[test]
    fn reusable_issued_uses_strict_renewal_boundary() {
        let mut store = TicketStore::default();
        let destination = [4; 16];
        let now = 10_000.0;
        let boundary = Ticket {
            expires_unix: now + TICKET_RENEW as f64,
            secret: [5; 16],
        };
        store.restore_inbound(destination, boundary.clone());
        assert!(store.reusable_issued(&destination, now).is_none());

        let reusable = Ticket {
            expires_unix: now + TICKET_RENEW as f64 + 1.0,
            secret: [6; 16],
        };
        store.restore_inbound(destination, reusable.clone());
        assert_eq!(store.reusable_issued(&destination, now), Some(&reusable));
        assert!(store.contains_inbound(&destination, &boundary));
        assert!(store.contains_inbound(&destination, &reusable));
        assert!(!store.contains_inbound(
            &destination,
            &Ticket {
                expires_unix: reusable.expires_unix,
                secret: [7; 16],
            }
        ));
    }

    #[test]
    fn issue_preserves_interval_and_reuse_boundaries() {
        let mut store = TicketStore::default();
        let destination = [8; 16];
        let first = store.issue(destination, 1_000.0, &mut OsRng).unwrap();
        assert_eq!(store.entry_count(), 1);

        store.mark_delivered(destination, 2_000.0);
        assert!(store
            .issue(
                destination,
                2_000.0 + TICKET_INTERVAL as f64 - 1.0,
                &mut OsRng,
            )
            .is_none());
        assert_eq!(store.entry_count(), 2);

        let reused = store
            .issue(destination, 2_000.0 + TICKET_INTERVAL as f64, &mut OsRng)
            .unwrap();
        assert_eq!(reused, first);
        assert_eq!(store.entry_count(), 2);
        assert!(store.issue(destination, f64::NAN, &mut OsRng).is_none());
    }

    #[test]
    fn validates_only_unexpired_owned_ticket_stamps() {
        let mut store = TicketStore::default();
        let source = [9; 16];
        let message_id = [10; 32];
        let ticket = Ticket {
            expires_unix: 200.0,
            secret: [11; 16],
        };
        store.restore_inbound(source, ticket.clone());
        let stamp = crate::stamp::ticket_stamp(&ticket.secret, &message_id);

        assert!(store.validates_inbound_stamp(&source, &message_id, &stamp, 199.999));
        assert!(!store.validates_inbound_stamp(&source, &message_id, &stamp, 200.0));
        assert!(!store.validates_inbound_stamp(&source, &[12; 32], &stamp, 199.0));
        assert!(!store.validates_inbound_stamp(&[13; 16], &message_id, &stamp, 199.0));
        assert!(!store.validates_inbound_stamp(&source, &message_id, &[0; 15], 199.0));
        assert!(!store.validates_inbound_stamp(&source, &message_id, &stamp, f64::NAN));
    }
}
