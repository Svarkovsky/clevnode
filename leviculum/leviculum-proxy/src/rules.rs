use std::fmt;

/// Default seed for a rule's probabilistic-loss PRNG when a scenario does not
/// specify one. A fixed default keeps rate-mode runs reproducible out of the
/// box: the same rule string always drops the same frames.
pub const DEFAULT_LOSS_SEED: u64 = 0x5EED_0000_0000_0001;

/// Deterministic seeded PRNG (SplitMix64) driving probabilistic loss.
///
/// Chosen over system randomness so a given `(seed, rate, frame sequence)` is
/// bit-for-bit reproducible: rate-mode tests must never be flaky. The
/// generator is self-contained (no external crate) to keep the proxy's
/// dependency surface minimal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform `f64` in the half-open range `[0.0, 1.0)`, built from the top
    /// 53 bits so every representable double is reachable and the value is
    /// always strictly less than 1.0.
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    AToB,
    BToA,
    Both,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::AToB => write!(f, "a_to_b"),
            Direction::BToA => write!(f, "b_to_a"),
            Direction::Both => write!(f, "both"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Drop,
    Delay(u64),
    Corrupt,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Drop => write!(f, "drop"),
            Action::Delay(ms) => write!(f, "delay {ms}"),
            Action::Corrupt => write!(f, "corrupt"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Command(u8),
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Filter::All => write!(f, "all"),
            Filter::Command(cmd) => write!(f, "cmd:0x{cmd:02x}"),
        }
    }
}

pub struct Rule {
    pub id: u32,
    pub direction: Direction,
    pub action: Action,
    pub filter: Filter,
    /// Only match frames with payload >= this size (0 = no minimum).
    pub min_size: usize,
    /// Only match frames with payload <= this size (0 = no maximum).
    pub max_size: usize,
    /// Number of matching frames to skip before the rule activates.
    pub skip: u32,
    /// Deterministic count mode: number of matching frames still to act on.
    /// `None` means "act on every match forever". Mutually exclusive with
    /// `rate`.
    pub remaining: Option<u32>,
    /// Probabilistic loss mode: when `Some(p)`, each matching frame (after
    /// `skip`) triggers the action with probability `p`, drawn from `rng`.
    /// `None` selects the deterministic `remaining` count mode instead.
    pub rate: Option<f64>,
    /// Seeded PRNG backing probabilistic loss. Only consulted when `rate` is
    /// `Some`; carried unconditionally so a `Rule` needs no extra allocation.
    rng: SplitMix64,
}

/// Full specification for a fault-injection rule.
///
/// `count` (deterministic) and `rate` (probabilistic) are mutually exclusive:
/// callers must set at most one. Construct via [`RuleSpec::default`] and set
/// only the fields that differ from the defaults.
pub struct RuleSpec {
    pub direction: Direction,
    pub action: Action,
    pub filter: Filter,
    pub min_size: usize,
    pub max_size: usize,
    pub skip: u32,
    /// Deterministic drop-count. `None` = act on every match.
    pub count: Option<u32>,
    /// Probabilistic loss rate in `0.0..=1.0`. `None` = deterministic mode.
    pub rate: Option<f64>,
    /// Seed for the probabilistic PRNG; defaults to `DEFAULT_LOSS_SEED`.
    pub seed: u64,
}

impl Default for RuleSpec {
    fn default() -> Self {
        Self {
            direction: Direction::Both,
            action: Action::Drop,
            filter: Filter::All,
            min_size: 0,
            max_size: 0,
            skip: 0,
            count: None,
            rate: None,
            seed: DEFAULT_LOSS_SEED,
        }
    }
}

pub struct KissFrame {
    pub command: u8,
    pub payload: Vec<u8>,
}

pub enum FrameDecision {
    Forward,
    Drop,
    Delay(u64),
    Corrupt(Vec<u8>),
}

pub struct RuleEngine {
    rules: Vec<Rule>,
    next_id: u32,
    pub forwarded: u64,
    pub dropped: u64,
    pub delayed: u64,
    pub corrupted: u64,
}

impl Default for RuleEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleEngine {
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            next_id: 1,
            forwarded: 0,
            dropped: 0,
            delayed: 0,
            corrupted: 0,
        }
    }

    /// Add a deterministic count-mode rule (probabilistic `rate` unset).
    /// Thin wrapper over [`RuleEngine::add_rule_spec`] preserving the original
    /// call shape used throughout the suite.
    #[allow(clippy::too_many_arguments)]
    pub fn add_rule(
        &mut self,
        direction: Direction,
        action: Action,
        filter: Filter,
        min_size: usize,
        max_size: usize,
        skip: u32,
        count: Option<u32>,
    ) -> u32 {
        self.add_rule_spec(RuleSpec {
            direction,
            action,
            filter,
            min_size,
            max_size,
            skip,
            count,
            ..RuleSpec::default()
        })
    }

    /// Add a rule from a full [`RuleSpec`], supporting both deterministic
    /// count mode (`spec.count`) and probabilistic loss mode (`spec.rate`).
    pub fn add_rule_spec(&mut self, spec: RuleSpec) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.rules.push(Rule {
            id,
            direction: spec.direction,
            action: spec.action,
            filter: spec.filter,
            min_size: spec.min_size,
            max_size: spec.max_size,
            skip: spec.skip,
            remaining: spec.count,
            rate: spec.rate,
            rng: SplitMix64::new(spec.seed),
        });
        id
    }

    pub fn clear_rule(&mut self, id: u32) -> bool {
        let len_before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.rules.len() < len_before
    }

    pub fn clear_all(&mut self) {
        self.rules.clear();
    }

    pub fn list_rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Evaluate rules for a frame. First matching rule wins.
    /// If the winning rule still has skip > 0, the frame is forwarded and
    /// skip is decremented. Once skip reaches 0, the action is applied.
    /// Decrements remaining count; auto-removes rules that reach 0.
    pub fn evaluate(&mut self, frame: &KissFrame, dir: Direction) -> FrameDecision {
        let mut matched_idx = None;

        for (i, rule) in self.rules.iter().enumerate() {
            let dir_match = rule.direction == Direction::Both || rule.direction == dir;
            if !dir_match {
                continue;
            }

            let filter_match = match rule.filter {
                Filter::All => true,
                Filter::Command(cmd) => frame.command == cmd,
            };
            if !filter_match {
                continue;
            }

            if rule.min_size > 0 && frame.payload.len() < rule.min_size {
                continue;
            }

            if rule.max_size > 0 && frame.payload.len() > rule.max_size {
                continue;
            }

            matched_idx = Some(i);
            break;
        }

        let Some(idx) = matched_idx else {
            self.forwarded += 1;
            return FrameDecision::Forward;
        };

        let rule = &mut self.rules[idx];

        // If skip > 0, forward the frame and decrement skip
        if rule.skip > 0 {
            rule.skip -= 1;
            self.forwarded += 1;
            return FrameDecision::Forward;
        }

        // Probabilistic loss mode: draw once per matching frame from the
        // seeded PRNG. draw < rate acts, otherwise forwards. The draw always
        // advances the PRNG so the dropped-frame set is a pure function of
        // (seed, rate, frame sequence). rate == 0.0 never acts (draw is in
        // [0,1)), rate == 1.0 always acts.
        if let Some(rate) = rule.rate {
            if rule.rng.next_f64() >= rate {
                self.forwarded += 1;
                return FrameDecision::Forward;
            }
        }

        let decision = match rule.action {
            Action::Drop => {
                self.dropped += 1;
                FrameDecision::Drop
            }
            Action::Delay(ms) => {
                self.delayed += 1;
                FrameDecision::Delay(ms)
            }
            Action::Corrupt => {
                self.corrupted += 1;
                let mut corrupted = frame.payload.clone();
                if !corrupted.is_empty() {
                    corrupted[0] ^= 0xFF;
                }
                FrameDecision::Corrupt(corrupted)
            }
        };

        // Decrement remaining count, auto-remove at 0
        if let Some(ref mut remaining) = rule.remaining {
            *remaining -= 1;
            if *remaining == 0 {
                self.rules.remove(idx);
            }
        }

        decision
    }

    pub fn stats_json(&self) -> String {
        format!(
            r#"{{"forwarded":{},"dropped":{},"delayed":{},"corrupted":{},"rules":{}}}"#,
            self.forwarded,
            self.dropped,
            self.delayed,
            self.corrupted,
            self.rules.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rules_forwards() {
        let mut engine = RuleEngine::new();
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1, 2, 3],
        };
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Forward
        ));
        assert_eq!(engine.forwarded, 1);
    }

    #[test]
    fn drop_rule() {
        let mut engine = RuleEngine::new();
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 0, None);
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1, 2, 3],
        };
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        assert_eq!(engine.dropped, 1);
        assert_eq!(engine.forwarded, 0);
    }

    #[test]
    fn drop_with_count() {
        let mut engine = RuleEngine::new();
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 0, Some(2));
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };

        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        // Count exhausted, rule auto-removed
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Forward
        ));
        assert_eq!(engine.dropped, 2);
        assert_eq!(engine.forwarded, 1);
        assert_eq!(engine.rule_count(), 0);
    }

    #[test]
    fn direction_filter() {
        let mut engine = RuleEngine::new();
        engine.add_rule(Direction::AToB, Action::Drop, Filter::All, 0, 0, 0, None);
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };

        // A->B matches
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        // B->A does not match
        assert!(matches!(
            engine.evaluate(&frame, Direction::BToA),
            FrameDecision::Forward
        ));
    }

    #[test]
    fn command_filter() {
        let mut engine = RuleEngine::new();
        engine.add_rule(
            Direction::Both,
            Action::Drop,
            Filter::Command(0x00),
            0,
            0,
            0,
            None,
        );

        let data_frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };
        let other_frame = KissFrame {
            command: 0x08,
            payload: vec![1],
        };

        assert!(matches!(
            engine.evaluate(&data_frame, Direction::AToB),
            FrameDecision::Drop
        ));
        assert!(matches!(
            engine.evaluate(&other_frame, Direction::AToB),
            FrameDecision::Forward
        ));
    }

    #[test]
    fn corrupt_flips_first_byte() {
        let mut engine = RuleEngine::new();
        engine.add_rule(Direction::Both, Action::Corrupt, Filter::All, 0, 0, 0, None);
        let frame = KissFrame {
            command: 0x00,
            payload: vec![0xAB, 0xCD],
        };

        match engine.evaluate(&frame, Direction::AToB) {
            FrameDecision::Corrupt(data) => {
                assert_eq!(data[0], 0xAB ^ 0xFF);
                assert_eq!(data[1], 0xCD); // second byte unchanged
            }
            _ => panic!("Expected Corrupt"),
        }
    }

    #[test]
    fn delay_returns_ms() {
        let mut engine = RuleEngine::new();
        engine.add_rule(
            Direction::Both,
            Action::Delay(150),
            Filter::All,
            0,
            0,
            0,
            None,
        );
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };

        match engine.evaluate(&frame, Direction::AToB) {
            FrameDecision::Delay(ms) => assert_eq!(ms, 150),
            _ => panic!("Expected Delay"),
        }
        assert_eq!(engine.delayed, 1);
    }

    #[test]
    fn first_matching_rule_wins() {
        let mut engine = RuleEngine::new();
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 0, None);
        engine.add_rule(Direction::Both, Action::Corrupt, Filter::All, 0, 0, 0, None);
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };

        // Drop rule matches first
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
    }

    #[test]
    fn clear_rule_by_id() {
        let mut engine = RuleEngine::new();
        let id = engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 0, None);
        assert!(engine.clear_rule(id));
        assert!(!engine.clear_rule(id)); // already removed
        assert_eq!(engine.rule_count(), 0);
    }

    #[test]
    fn clear_all() {
        let mut engine = RuleEngine::new();
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 0, None);
        engine.add_rule(Direction::Both, Action::Corrupt, Filter::All, 0, 0, 0, None);
        engine.clear_all();
        assert_eq!(engine.rule_count(), 0);
    }

    #[test]
    fn stats_json_format() {
        let mut engine = RuleEngine::new();
        engine.forwarded = 10;
        engine.dropped = 2;
        engine.delayed = 1;
        engine.corrupted = 3;
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 0, None);

        let json = engine.stats_json();
        assert_eq!(
            json,
            r#"{"forwarded":10,"dropped":2,"delayed":1,"corrupted":3,"rules":1}"#
        );
    }

    #[test]
    fn skip_forwards_before_acting() {
        let mut engine = RuleEngine::new();
        // Skip 2 matching frames, then drop the next 3
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 0, 0, 2, Some(3));
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };

        // First two are forwarded (skip)
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Forward
        ));
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Forward
        ));
        assert_eq!(engine.forwarded, 2);
        assert_eq!(engine.dropped, 0);

        // Next three are dropped
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Drop
        ));
        assert_eq!(engine.dropped, 3);

        // Rule exhausted, back to forwarding
        assert!(matches!(
            engine.evaluate(&frame, Direction::AToB),
            FrameDecision::Forward
        ));
        assert_eq!(engine.forwarded, 3);
        assert_eq!(engine.rule_count(), 0);
    }

    #[test]
    fn max_size_filter() {
        let mut engine = RuleEngine::new();
        // Drop frames with payload between 50 and 130 bytes
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 50, 130, 0, None);

        let small_frame = KissFrame {
            command: 0x00,
            payload: vec![0u8; 30],
        };
        let medium_frame = KissFrame {
            command: 0x00,
            payload: vec![0u8; 86],
        };
        let large_frame = KissFrame {
            command: 0x00,
            payload: vec![0u8; 200],
        };

        // 30 bytes: below min_size, forwarded
        assert!(matches!(
            engine.evaluate(&small_frame, Direction::AToB),
            FrameDecision::Forward
        ));
        // 86 bytes: within range, dropped
        assert!(matches!(
            engine.evaluate(&medium_frame, Direction::AToB),
            FrameDecision::Drop
        ));
        // 200 bytes: above max_size, forwarded
        assert!(matches!(
            engine.evaluate(&large_frame, Direction::AToB),
            FrameDecision::Forward
        ));

        assert_eq!(engine.dropped, 1);
        assert_eq!(engine.forwarded, 2);
    }

    /// Collect the indices of frames the engine dropped over a fixed-length
    /// sequence of identical matching frames.
    fn dropped_indices(spec: RuleSpec, n: usize) -> Vec<usize> {
        let mut engine = RuleEngine::new();
        engine.add_rule_spec(spec);
        let frame = KissFrame {
            command: 0x00,
            payload: vec![1],
        };
        (0..n)
            .filter(|_| {
                matches!(
                    engine.evaluate(&frame, Direction::AToB),
                    FrameDecision::Drop
                )
            })
            .collect()
    }

    #[test]
    fn rate_mode_is_reproducible() {
        // Same (seed, rate) over the same frame sequence => identical drops.
        let spec = || RuleSpec {
            action: Action::Drop,
            rate: Some(0.4),
            seed: 12345,
            ..RuleSpec::default()
        };
        let run_a = dropped_indices(spec(), 1000);
        let run_b = dropped_indices(spec(), 1000);
        assert_eq!(
            run_a, run_b,
            "seeded rate mode must be bit-for-bit repeatable"
        );
        // Sanity: rate=0.4 must actually drop a non-trivial, non-total subset.
        assert!(!run_a.is_empty());
        assert!(run_a.len() < 1000);
    }

    #[test]
    fn rate_mode_seed_changes_drop_set() {
        // A different seed must yield a different dropped-frame set, proving
        // the stream is seed-derived rather than fixed.
        let with_seed = |seed| RuleSpec {
            action: Action::Drop,
            rate: Some(0.5),
            seed,
            ..RuleSpec::default()
        };
        assert_ne!(
            dropped_indices(with_seed(1), 500),
            dropped_indices(with_seed(2), 500)
        );
    }

    #[test]
    fn rate_mode_statistical_fraction() {
        // Over a large N the drop fraction sits within a tight tolerance of
        // the configured rate. Deterministic (seeded), so this is not flaky.
        let n = 100_000;
        let rate = 0.3;
        let dropped = dropped_indices(
            RuleSpec {
                action: Action::Drop,
                rate: Some(rate),
                seed: 0xABCDEF,
                ..RuleSpec::default()
            },
            n,
        )
        .len();
        let fraction = dropped as f64 / n as f64;
        assert!(
            (fraction - rate).abs() < 0.01,
            "drop fraction {fraction} deviates from rate {rate} beyond tolerance"
        );
    }

    #[test]
    fn rate_zero_drops_nothing_and_one_drops_all() {
        assert_eq!(
            dropped_indices(
                RuleSpec {
                    action: Action::Drop,
                    rate: Some(0.0),
                    ..RuleSpec::default()
                },
                500,
            )
            .len(),
            0
        );
        assert_eq!(
            dropped_indices(
                RuleSpec {
                    action: Action::Drop,
                    rate: Some(1.0),
                    ..RuleSpec::default()
                },
                500,
            )
            .len(),
            500
        );
    }

    #[test]
    fn rate_mode_respects_skip() {
        // The first `skip` matching frames are forwarded before any draw.
        let dropped = dropped_indices(
            RuleSpec {
                action: Action::Drop,
                rate: Some(1.0),
                skip: 3,
                ..RuleSpec::default()
            },
            10,
        );
        // rate=1.0 drops every post-skip frame => indices 3..10.
        assert_eq!(dropped, vec![3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn count_mode_unchanged_via_spec() {
        // Deterministic count mode still bounds and auto-removes exactly as
        // before when routed through the spec path.
        let dropped = dropped_indices(
            RuleSpec {
                action: Action::Drop,
                count: Some(2),
                ..RuleSpec::default()
            },
            10,
        );
        assert_eq!(dropped, vec![0, 1]);
    }

    #[test]
    fn max_size_zero_means_no_limit() {
        let mut engine = RuleEngine::new();
        // max_size=0 means no upper bound
        engine.add_rule(Direction::Both, Action::Drop, Filter::All, 50, 0, 0, None);

        let large_frame = KissFrame {
            command: 0x00,
            payload: vec![0u8; 10000],
        };

        // Should match since max_size=0 imposes no limit
        assert!(matches!(
            engine.evaluate(&large_frame, Direction::AToB),
            FrameDecision::Drop
        ));
    }
}
