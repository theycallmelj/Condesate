//! Cross-node cache sharing: advertise metadata, move values on purpose.
//!
//! This is the Pluribus-shaped layer. The whole design rests on one split:
//!
//! ```text
//!   control plane   compact ownership sketches, gossiped freely      cheap
//!   data plane      whole values, pulled only when a planner asks    expensive
//! ```
//!
//! A node never broadcasts values and never asks a central owner where things
//! are. It publishes a bounded [`RangeSketch`] describing *roughly what it
//! holds*, reads its peers' sketches, and when a local miss looks like it could
//! be repaired from a peer, a [`PullPlanner`] decides whether the value is
//! worth the bytes. The answer is usually no; that is the point.
//!
//! ## Why the metadata is lossy on purpose
//!
//! A sketch is a summary — centroid, radius, shape, counts, hotness — not an
//! index. It fits in a fixed byte budget no matter how much the node holds,
//! which is what keeps gossip cost flat as the swarm grows. The cost is false
//! positives: a planner sometimes pulls a range that turns out not to help.
//! That is a bandwidth bug, not a correctness bug — every pulled value is
//! revalidated on arrival.
//!
//! ## Where the permission layer cuts in
//!
//! Three separate checks, none of which the cache layer decides for itself:
//!
//! * `Action::Publish` — may this node advertise this pool's contents at all?
//!   Sketches leak *shape*: centroids describe what a tenant is working on.
//! * `Action::Pull` — may this principal move bytes from that peer?
//! * `Action::Read` — may this principal see the label on what came back?
//!   Applied per item, after arrival, before the value reaches a model.

use super::entry::{CacheEntry, CacheKey, Demand, RejectReason, Sidecar, ValueClass};
use crate::security::identity::{GovernanceLabel, ModelClass};
use crate::types::HarnessId;
use anyhow::Result;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// Sketches
// ---------------------------------------------------------------------------

/// A compact summary of one cluster of nearby values held by one node.
///
/// Deliberately holds no keys and no values — only enough geometry to answer
/// "might this peer have something near my query?".
#[derive(Clone, Debug)]
pub struct Range {
    /// Normalized centroid of the member embeddings, weighted by hotness so
    /// frequently-reused entries pull the summary toward themselves.
    pub centroid: Vec<f32>,
    /// Coarse outer envelope: the p95 member distance from the centroid.
    pub radius: f32,
    /// Optional low-rank shape. Without it the range is a sphere, which
    /// over-advertises for elongated clusters; with it, fewer wasted pulls.
    pub shape: Option<Ellipsoid>,
    pub value_class: ValueClass,
    pub entries: u32,
    pub bytes: u64,
    /// Aggregate reuse, for ranking.
    pub hotness: u32,
    /// Oldest and newest member write times; a planner skips stale ranges.
    pub oldest_ms: u64,
    pub newest_ms: u64,
    /// Labels present in this range. A peer that cannot legally read any of
    /// them skips the range without a round trip.
    pub labels: Vec<GovernanceLabel>,
}

/// Low-rank shape descriptor: a few principal axes plus a residual radius for
/// every direction not explicitly advertised.
///
/// The residual is what makes truncation safe. Dropping axes to fit the budget
/// makes the range *wider*, never narrower, so a shrunk sketch can cost extra
/// pulls but can never hide a value that was actually there.
#[derive(Clone, Debug)]
pub struct Ellipsoid {
    /// Retained principal directions, `k` rows of `dim` floats.
    pub axes: Vec<Vec<f32>>,
    /// Per-axis radius.
    pub axis_radii: Vec<f32>,
    /// Radius charged across all omitted dimensions.
    pub residual_radius: f32,
    /// Admission threshold fitted from members.
    pub threshold: f32,
}

/// Everything one node advertises about one pool.
#[derive(Clone, Debug)]
pub struct RangeSketch {
    pub owner: HarnessId,
    pub class: ModelClass,
    pub ranges: Vec<Range>,
    /// Wall-clock of publication. Consumers discount stale sketches.
    pub published_at_ms: u64,
    /// Serialized size, against [`SketchBudget::max_bytes`].
    pub bytes: u32,
}

/// The hard ceiling a sketch must fit inside, and how to degrade to reach it.
///
/// Degradation order matters: drop precision before dropping coverage. Losing
/// an axis widens a range; losing a range hides values entirely.
#[derive(Clone, Copy, Debug)]
pub struct SketchBudget {
    /// Hard cap on the serialized sketch. Exceeding it fails closed — publish
    /// a smaller sketch, never an oversized one.
    pub max_bytes: u32,
    /// Soft target the builder aims for before degrading.
    pub target_bytes: u32,
    /// Cap on advertised ranges.
    pub max_ranges: u16,
    /// Cap on retained principal axes per range.
    pub max_axes: u8,
    /// Ranges below this occupancy are merged into a neighbour rather than
    /// advertised separately — many tiny ranges waste the budget and each one
    /// is a near-useless advertisement.
    pub min_entries_per_range: u16,
}

impl Default for SketchBudget {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024,
            target_bytes: 12 * 1024,
            max_ranges: 64,
            max_axes: 4,
            min_entries_per_range: 4,
        }
    }
}

/// Turns live sidecars into a bounded sketch.
#[async_trait]
pub trait SketchPublisher: Send + Sync {
    /// Build a sketch for one pool. Must respect [`SketchBudget::max_bytes`]
    /// or return an error — never an oversized payload.
    async fn build(
        &self,
        class: &ModelClass,
        sidecars: &[Sidecar],
        budget: SketchBudget,
    ) -> Result<RangeSketch>;
}

/// What a node knows about its peers' holdings.
///
/// In this codebase the transport underneath is the [`crate::swarm::bus::Bus`]; across
/// machines it would be a gossip layer. Either way the directory is
/// eventually-consistent and always incomplete, and the planner must behave
/// sanely when it is stale.
#[async_trait]
pub trait SketchDirectory: Send + Sync {
    /// Advertise a local sketch to peers.
    async fn publish(&self, sketch: RangeSketch) -> Result<()>;

    /// Peer sketches for one class, freshest first, excluding our own.
    async fn peers_for(&self, class: &ModelClass) -> Result<Vec<RangeSketch>>;

    /// Withdraw everything this node advertised. Called on shutdown and on
    /// revocation, so peers stop planning against a pool they can no longer
    /// legally read.
    async fn withdraw(&self, owner: &HarnessId) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// One decision to move bytes: pull *these* items from *this* peer.
#[derive(Clone, Debug)]
pub struct PullPlan {
    pub peer: HarnessId,
    pub class: ModelClass,
    /// Which advertised range this targets, as an index into the peer sketch.
    pub range_index: usize,
    /// Exact keys, when known. Empty means "whatever is nearest the demand".
    pub keys: Vec<CacheKey>,
    /// Upper bound on returned items.
    pub max_items: u16,
    /// Upper bound on returned bytes. Enforced by the receiver too — a peer
    /// that overshoots is rejected, not trusted.
    pub max_bytes: u64,
    /// The planner's own estimate, kept for the audit record so a bad policy is
    /// diagnosable after the fact.
    pub expected_utility: f32,
}

/// Everything a planner needs that it cannot derive from the sketches.
#[derive(Clone)]
pub struct PlanInputs<'a> {
    pub demand: &'a Demand,
    pub peers: &'a [RangeSketch],
    /// Bytes this round may move, from [`super::pool::PoolUsage::round_budget`].
    pub byte_budget: u64,
    /// Peer reliability, from validation history.
    pub quality: &'a dyn PeerQuality,
    pub now_ms: u64,
}

/// Decides what is worth pulling.
///
/// A planner should be judged on what it *declines*. Four properties, in rough
/// order of how much they matter:
///
/// * **aligned** — near the actual demand, not merely near something
/// * **novel** — covers a direction the local pool does not already hold
/// * **diverse** — two candidate ranges pointing the same way are one pull
/// * **affordable** — inside the byte budget, after the local reserve
#[async_trait]
pub trait PullPlanner: Send + Sync {
    /// Zero plans is a normal, common, good answer.
    async fn plan(&self, inputs: PlanInputs<'_>) -> Result<Vec<PullPlan>>;
}

/// Tracks which peers return usable values.
///
/// Every [`RejectReason`] is evidence. A peer whose responses keep failing
/// validation should be planned against less, which makes a compromised or
/// buggy node fade out without anyone having to notice manually.
pub trait PeerQuality: Send + Sync {
    /// `0.0` (useless) to `1.0` (reliable). Multiplies planner utility.
    fn score(&self, peer: &HarnessId) -> f32;

    fn record_success(&self, peer: &HarnessId, items: u32, bytes: u64);

    fn record_rejection(&self, peer: &HarnessId, reason: &RejectReason);

    /// Peers below the usable threshold. Excluded from planning entirely.
    fn quarantined(&self) -> Vec<HarnessId>;
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A bounded request for values. The wire form of a [`PullPlan`].
#[derive(Clone, Debug)]
pub struct PullRequest {
    pub from: HarnessId,
    pub class: ModelClass,
    pub keys: Vec<CacheKey>,
    /// Query vector when the pull is a near-neighbour repair rather than an
    /// exact fetch.
    pub near: Option<Vec<f32>>,
    pub max_items: u16,
    pub max_bytes: u64,
    /// Set when the query vector was *predicted* (moved toward a peer group)
    /// rather than observed. The responder validates against its own sidecar
    /// embeddings instead of trusting the vector as a literal demand.
    pub projected: bool,
    /// Labels the requester is cleared for, so the responder can filter before
    /// serializing. A convenience, not the enforcement point — the receiver
    /// re-checks every item.
    pub cleared_labels: Vec<GovernanceLabel>,
}

/// A bounded response: candidate items, never a range dump.
#[derive(Clone, Debug)]
pub struct PullResponse {
    pub from: HarnessId,
    pub items: Vec<CacheEntry>,
    /// True when the peer had more but hit the request's bounds.
    pub truncated: bool,
}

/// Moves values between nodes.
///
/// In-process this wraps the bus; across machines it is an RPC client. Either
/// way: bounded requests, validated responses, no implicit trust in the peer.
#[async_trait]
pub trait PeerTransport: Send + Sync {
    async fn pull(&self, to: &HarnessId, req: PullRequest) -> Result<PullResponse>;

    /// Serve an inbound pull from our own pools. Applies the requester's
    /// clearance and the request's bounds; a responder that overshoots gets
    /// rejected on the far side, so overshooting is only ever self-harm.
    async fn serve(&self, req: &PullRequest) -> Result<PullResponse>;
}

/// What a completed pull did. Recorded per plan so the loop is diagnosable:
/// a planner that keeps producing rejected pulls is visible in the numbers.
#[derive(Clone, Debug, Default)]
pub struct PullOutcome {
    pub requested_items: u32,
    pub returned_items: u32,
    pub admitted_items: u32,
    pub rejected: Vec<RejectReason>,
    pub bytes_moved: u64,
    pub latency_ms: u64,
}
