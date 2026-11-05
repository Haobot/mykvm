//! P4 cross-machine focus protocol ("mykvm.halo.v1"): structured messages a
//! halo-sender machine emits when its halo reaches a screen edge, and the
//! receiver-side throttle that keeps the stream at state-change-only rate.
//!
//! Transport is a QUIC datagram, encoded with `rmp_serde::to_vec_named` and
//! serde camelCase like every other packet in this crate. Forward/backward
//! compatibility follows the input-packet convention: the `kind` tag makes an
//! unknown future variant fail decoding on an old peer, and the packet is then
//! silently dropped by the input datagram chain (decode returns `None`), which
//! is the accepted behavior for mixed-version pairs.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Protocol identifier carried in every packet; mismatched packets are
/// dropped at decode so unrelated datagram payloads can never masquerade as
/// halo focus traffic.
pub const HALO_FOCUS_PROTOCOL: &str = "mykvm.halo.v1";

/// Identical events inside this window are never resent (burst guard).
pub const RESEND_SUPPRESS: Duration = Duration::from_millis(150);
/// Identical events beyond the suppress window are still throttled to this
/// slower heartbeat: the receiver auto-clears its highlight after 5 s, so an
/// unchanged state (a long dwell) must be refreshed to stay visible.
pub const UNCHANGED_REFRESH: Duration = Duration::from_secs(1);
/// Coordinate quantum (layout-board logical pixels) for the "same event"
/// comparison: a halo parked on a window jitters by a few pixels, and those
/// micro-moves must not count as state changes.
pub const COORD_QUANTUM: f64 = 5.0;

/// One focus event directed at a peer screen. `screen_id` is the target
/// screen's id from the shared layout board; `x`/`y` are the landing point in
/// layout-board logical coordinates (the shared coordinate space every
/// machine's screens are announced into), so the receiver can hit-test the
/// point against its own windows directly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum HaloFocusEvent {
    /// The halo reached the peer screen's edge: the peer shows a passive
    /// highlight at the landing point.
    #[serde(rename_all = "camelCase")]
    Edge { screen_id: String, x: f64, y: f64 },
    /// The halo is dwelling over a spot on the peer screen; the peer
    /// highlights whatever window is under the point.
    #[serde(rename_all = "camelCase")]
    Candidate { screen_id: String, x: f64, y: f64 },
    /// Dwell threshold reached: the peer highlights the window and activates
    /// it.
    #[serde(rename_all = "camelCase")]
    Activate { screen_id: String, x: f64, y: f64 },
    /// S4 cross-machine takeover landing: the sender's head-turn engine
    /// committed a region whose screen belongs to the receiver. The receiver
    /// warps its own cursor to (x, y) (board coordinates) and activates the
    /// window under the point — an `Activate` plus a pointer warp, marking
    /// the moment control intent moved to this machine. Input still flows
    /// through the normal controller path afterwards (see the pump-side
    /// dispatch notes in lib.rs); Land only places the peer's cursor and
    /// focus where the user's head already is.
    #[serde(rename_all = "camelCase")]
    Land { screen_id: String, x: f64, y: f64 },
    /// The halo left the edge (or the sender's pipeline stopped): the peer
    /// clears its highlight.
    Clear,
}

impl HaloFocusEvent {
    /// Stable dedupe key: variant tag plus the coordinates quantized to
    /// [`COORD_QUANTUM`]. `Clear` carries no coordinates.
    pub fn dedupe_key(&self) -> (&'static str, Option<(i64, i64)>) {
        let point = |x: f64, y: f64| {
            Some((
                (x / COORD_QUANTUM).round() as i64,
                (y / COORD_QUANTUM).round() as i64,
            ))
        };
        match self {
            HaloFocusEvent::Edge { x, y, .. } => ("edge", point(*x, *y)),
            HaloFocusEvent::Candidate { x, y, .. } => ("candidate", point(*x, *y)),
            HaloFocusEvent::Activate { x, y, .. } => ("activate", point(*x, *y)),
            HaloFocusEvent::Land { x, y, .. } => ("land", point(*x, *y)),
            HaloFocusEvent::Clear => ("clear", None),
        }
    }
}

/// Envelope for one [`HaloFocusEvent`]. The credentials ride on every packet
/// (the stream is <= 10 Hz state-change traffic, so the input-plane's
/// fresh-authorization cache is not worth the state): the receiver authorizes
/// the full credential block exactly like an input packet carrying
/// credentials.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HaloFocusPacket {
    pub protocol: String,
    pub source_device_id: String,
    pub cluster_id: String,
    pub pair_secret: String,
    pub event: HaloFocusEvent,
    pub seq: u64,
}

impl HaloFocusPacket {
    /// Build a packet with the protocol string filled in.
    pub fn new(
        source_device_id: String,
        cluster_id: String,
        pair_secret: String,
        event: HaloFocusEvent,
        seq: u64,
    ) -> Self {
        HaloFocusPacket {
            protocol: HALO_FOCUS_PROTOCOL.into(),
            source_device_id,
            cluster_id,
            pair_secret,
            event,
            seq,
        }
    }

    /// MessagePack encode, the crate-wide datagram convention.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        rmp_serde::to_vec_named(self).map_err(|error| format!("encode halo focus packet: {error}"))
    }
}

/// Decode one datagram payload as a halo focus packet. Returns `None` for
/// anything that is not one: wrong schema, wrong protocol string, or an event
/// variant this build does not know (a newer peer talking to an older peer —
/// the input datagram chain then drops the payload silently, which is the
/// accepted mixed-version behavior).
pub fn decode_halo_focus_packet(bytes: &[u8]) -> Option<HaloFocusPacket> {
    let packet: HaloFocusPacket = rmp_serde::from_slice(bytes).ok()?;
    if packet.protocol != HALO_FOCUS_PROTOCOL {
        return None;
    }
    Some(packet)
}

/// Sender-side throttle and sequence allocation. The halo engine produces a
/// decision every tick; this filter turns that stream into the <= 10 Hz,
/// state-change-only wire traffic the product requires.
#[derive(Debug, Default)]
pub struct FocusSender {
    last_sent: Option<(HaloFocusEvent, Instant)>,
    seq: u64,
}

impl FocusSender {
    pub fn new() -> Self {
        FocusSender {
            last_sent: None,
            seq: 0,
        }
    }

    /// Decide whether `event` may go out now, and allocate its sequence
    /// number. Rules:
    /// - a state change (different dedupe key) is always sent;
    /// - an identical event is suppressed for [`RESEND_SUPPRESS`], then kept
    ///   suppressed until [`UNCHANGED_REFRESH`] elapses (heartbeat refresh so
    ///   the peer's 5 s auto-clear does not fire mid-dwell);
    /// - every accepted send increments the sequence.
    ///
    /// The return value is the `seq` to stamp the packet with — everything
    /// else the envelope needs (device id, credentials) comes from the layout
    /// at send time.
    pub fn should_send(&mut self, event: &HaloFocusEvent) -> Option<u64> {
        self.should_send_at(event, Instant::now())
    }

    /// [`FocusSender::should_send`] with an explicit clock, so tests can
    /// exercise the time windows without sleeping.
    pub fn should_send_at(&mut self, event: &HaloFocusEvent, now: Instant) -> Option<u64> {
        if let Some((last_event, sent_at)) = self.last_sent.as_ref() {
            let elapsed = now.saturating_duration_since(*sent_at);
            if last_event.dedupe_key() == event.dedupe_key()
                && (elapsed < RESEND_SUPPRESS || elapsed < UNCHANGED_REFRESH)
            {
                return None;
            }
        }
        self.seq = self.seq.wrapping_add(1);
        self.last_sent = Some((event.clone(), now));
        Some(self.seq)
    }

    /// Sequence number of the last allocated packet (exposed for telemetry).
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(screen: &str, x: f64, y: f64) -> HaloFocusEvent {
        HaloFocusEvent::Edge {
            screen_id: screen.into(),
            x,
            y,
        }
    }

    #[test]
    fn roundtrips_camel_case_and_kind_tag() {
        let packet = HaloFocusPacket::new(
            "peer-a-1".into(),
            "cluster".into(),
            "secret".into(),
            HaloFocusEvent::Candidate {
                screen_id: "screen-2".into(),
                x: 120.0,
                y: 340.5,
            },
            7,
        );
        let bytes = packet.encode().expect("encode");
        let decoded = decode_halo_focus_packet(&bytes).expect("decode");
        assert_eq!(decoded, packet);
        // The on-disk shape must stay camelCase with the kind tag: pin both
        // field names and the tag value so an accidental rename cannot break
        // the wire format silently.
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("sourceDeviceId"), "camelCase field: {text}");
        assert!(text.contains("screenId"), "camelCase field: {text}");
        assert!(text.contains("candidate"), "kind tag: {text}");
    }

    #[test]
    fn clear_variant_roundtrips() {
        let packet =
            HaloFocusPacket::new("a".into(), "c".into(), "s".into(), HaloFocusEvent::Clear, 1);
        let decoded = decode_halo_focus_packet(&packet.encode().expect("encode")).expect("decode");
        assert_eq!(decoded.event, HaloFocusEvent::Clear);
    }

    #[test]
    fn land_variant_roundtrips_and_keeps_its_own_dedupe_space() {
        let land = HaloFocusEvent::Land {
            screen_id: "peer-s".into(),
            x: 120.0,
            y: 340.5,
        };
        let decoded = decode_halo_focus_packet(
            &HaloFocusPacket::new("a".into(), "c".into(), "s".into(), land.clone(), 3)
                .encode()
                .expect("encode"),
        )
        .expect("land roundtrips");
        assert_eq!(decoded.event, land);
        let wire = decoded.encode().expect("re-encode");
        let text = String::from_utf8_lossy(&wire);
        assert!(text.contains("land"), "kind tag in wire format: {text}");
        // Same point as an Activate: a different variant tag is a state
        // change, so a takeover landing right after a dwell activation is
        // never collapsed by the throttle.
        let activate = HaloFocusEvent::Activate {
            screen_id: "peer-s".into(),
            x: 120.0,
            y: 340.5,
        };
        assert_ne!(land.dedupe_key(), activate.dedupe_key());
        assert_eq!(
            land.dedupe_key(),
            HaloFocusEvent::Land {
                screen_id: "other".into(),
                x: 120.0,
                y: 340.5
            }
            .dedupe_key(),
            "coordinates dominate the key; screen_id rides in the payload"
        );
    }

    #[test]
    fn unknown_kind_fails_decode_silently() {
        // A future variant ("pin") is unknown to this build: decode must
        // return None (the input chain then drops it silently), never panic
        // or misparse. The payload is produced by a local mirror struct with
        // the exact wire shape of HaloFocusPacket but a newer event variant,
        // simulating a newer peer talking to this (older) build.
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase", tag = "kind")]
        enum FutureEvent {
            Pin { screen_id: String, x: f64, y: f64 },
        }
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct FuturePacket<'a> {
            protocol: &'a str,
            source_device_id: &'a str,
            cluster_id: &'a str,
            pair_secret: &'a str,
            event: FutureEvent,
            seq: u64,
        }
        let payload = rmp_serde::to_vec_named(&FuturePacket {
            protocol: HALO_FOCUS_PROTOCOL,
            source_device_id: "peer-x",
            cluster_id: "cluster",
            pair_secret: "secret",
            event: FutureEvent::Pin {
                screen_id: "s".into(),
                x: 1.0,
                y: 2.0,
            },
            seq: 9,
        })
        .expect("encode future packet");
        assert!(decode_halo_focus_packet(&payload).is_none());
    }

    #[test]
    fn wrong_protocol_string_is_rejected() {
        let mut packet =
            HaloFocusPacket::new("a".into(), "c".into(), "s".into(), HaloFocusEvent::Clear, 1);
        packet.protocol = "mykvm.input.v9".into();
        let bytes = packet.encode().expect("encode");
        assert!(decode_halo_focus_packet(&bytes).is_none());
    }

    #[test]
    fn truncated_payload_is_rejected() {
        let packet =
            HaloFocusPacket::new("a".into(), "c".into(), "s".into(), HaloFocusEvent::Clear, 1);
        let bytes = packet.encode().expect("encode");
        assert!(decode_halo_focus_packet(&bytes[..bytes.len() - 2]).is_none());
        assert!(decode_halo_focus_packet(&[]).is_none());
    }

    #[test]
    fn throttle_suppresses_identical_event_within_suppress_window() {
        let mut sender = FocusSender::new();
        let t0 = Instant::now();
        let event = edge("s", 100.0, 200.0);
        assert_eq!(sender.should_send_at(&event, t0), Some(1));
        // Same event 10 ms and 149 ms later: suppressed.
        assert_eq!(
            sender.should_send_at(&event, t0 + Duration::from_millis(10)),
            None
        );
        assert_eq!(
            sender.should_send_at(&event, t0 + Duration::from_millis(149)),
            None
        );
    }

    #[test]
    fn throttle_sends_on_any_change_immediately() {
        let mut sender = FocusSender::new();
        let t0 = Instant::now();
        assert_eq!(sender.should_send_at(&edge("s", 100.0, 200.0), t0), Some(1));
        // Coordinate change (beyond the 5 px quantum) is a state change.
        assert_eq!(
            sender.should_send_at(&edge("s", 130.0, 200.0), t0 + Duration::from_millis(10)),
            Some(2)
        );
        // Kind change is a state change even at the same point.
        let candidate = HaloFocusEvent::Candidate {
            screen_id: "s".into(),
            x: 130.0,
            y: 200.0,
        };
        assert_eq!(
            sender.should_send_at(&candidate, t0 + Duration::from_millis(20)),
            Some(3)
        );
        // Different target screen is a state change.
        assert_eq!(
            sender.should_send_at(&edge("other", 130.0, 200.0), t0 + Duration::from_millis(30)),
            Some(4)
        );
        // Clear is always a change after a non-clear.
        assert_eq!(
            sender.should_send_at(&HaloFocusEvent::Clear, t0 + Duration::from_millis(40)),
            Some(5)
        );
    }

    #[test]
    fn coordinates_quantize_to_five_logical_pixels() {
        // 100.0 and 102.4 round to the same 5 px bucket (20), so the second
        // send is suppressed; 102.6 rounds to bucket 21 (a state change).
        let mut sender = FocusSender::new();
        let t0 = Instant::now();
        assert_eq!(sender.should_send_at(&edge("s", 100.0, 200.0), t0), Some(1));
        assert_eq!(
            sender.should_send_at(&edge("s", 102.4, 202.4), t0 + Duration::from_millis(10)),
            None
        );
        assert_eq!(
            sender.should_send_at(&edge("s", 102.6, 200.0), t0 + Duration::from_millis(10)),
            Some(2)
        );
    }

    #[test]
    fn unchanged_state_refreshes_at_the_heartbeat_rate() {
        let mut sender = FocusSender::new();
        let t0 = Instant::now();
        let event = edge("s", 100.0, 200.0);
        assert_eq!(sender.should_send_at(&event, t0), Some(1));
        // Between the suppress window and the heartbeat: still suppressed
        // (deviation from the bare 150 ms rule, keeping identical-state
        // traffic at 1 Hz — see UNCHANGED_REFRESH).
        assert_eq!(
            sender.should_send_at(&event, t0 + Duration::from_millis(300)),
            None
        );
        assert_eq!(
            sender.should_send_at(&event, t0 + UNCHANGED_REFRESH),
            Some(2)
        );
    }

    #[test]
    fn seq_increments_monotonically() {
        let mut sender = FocusSender::new();
        let t0 = Instant::now();
        let mut seq = 0;
        for _ in 0..4 {
            seq = sender
                .should_send_at(&edge("s", seq as f64 * 100.0, 0.0), t0)
                .expect("each distinct point sends");
        }
        assert_eq!(seq, 4);
        assert_eq!(sender.seq(), 4);
    }

    #[test]
    fn constants_match_design() {
        assert_eq!(HALO_FOCUS_PROTOCOL, "mykvm.halo.v1");
        assert_eq!(RESEND_SUPPRESS, Duration::from_millis(150));
        assert_eq!(UNCHANGED_REFRESH, Duration::from_secs(1));
        assert!((COORD_QUANTUM - 5.0).abs() < f64::EPSILON);
    }
}
