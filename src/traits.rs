//! Common supertraits for distributed algorithms.

use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::iter::once;

use failure::Fail;
use serde::{de::DeserializeOwned, Serialize};

use fault_log::{Fault, FaultLog};
use sender_queue::SenderQueueableMessage;
use {Target, TargetedMessage};

/// A transaction, user message, or other user data.
pub trait Contribution: Eq + Debug + Hash + Send + Sync {}
impl<C> Contribution for C where C: Eq + Debug + Hash + Send + Sync {}

/// A peer node's unique identifier.
pub trait NodeIdT: Eq + Ord + Clone + Debug + Hash + Send + Sync {}
impl<N> NodeIdT for N where N: Eq + Ord + Clone + Debug + Hash + Send + Sync {}

/// Messages.
pub trait Message: Debug + Send + Sync {}
impl<M> Message for M where M: Debug + Send + Sync {}

/// Session identifiers.
pub trait SessionIdT: Display + Serialize + Send + Sync + Clone + Debug {}
impl<S> SessionIdT for S where S: Display + Serialize + Send + Sync + Clone + Debug {}

/// Epochs.
pub trait EpochT: Copy + Message + Default + Eq + Ord + Serialize + DeserializeOwned {}
impl<E> EpochT for E where E: Copy + Message + Default + Eq + Ord + Serialize + DeserializeOwned {}

/// Single algorithm step outcome.
///
/// Each time input (typically in the form of user input or incoming network messages) is provided
/// to an instance of an algorithm, a `Step` is produced, potentially containing output values,
/// a fault log, and network messages.
///
/// Any `Step` **must always be used** by the client application; at the very least the resulting
/// messages must be queued.
///
/// ## Handling unused Steps
///
/// In the (rare) case of a `Step` not being of any interest at all, instead of discarding it
/// through `let _ = ...` or similar constructs, the implicit assumption should explicitly be
/// checked instead:
///
/// ```ignore
/// assert!(alg.propose(123).expect("Could not propose value").is_empty(),
///         "Algorithm will never output anything on first proposal");
/// ```
///
/// If an edge case occurs and outgoing messages are generated as a result, the `assert!` will
/// catch it, instead of potentially stalling the algorithm.
#[must_use = "The algorithm step result must be used."]
#[derive(Debug)]
pub struct Step<M, O, N> {
    pub output: Vec<O>,
    pub fault_log: FaultLog<N>,
    pub messages: Vec<TargetedMessage<M, N>>,
}

impl<M, O, N> Default for Step<M, O, N> {
    fn default() -> Self {
        Step {
            output: Vec::default(),
            fault_log: FaultLog::default(),
            messages: Vec::default(),
        }
    }
}

impl<M, O, N> Step<M, O, N> {
    /// Creates a new `Step` from the given collections.
    pub fn new(
        output: Vec<O>,
        fault_log: FaultLog<N>,
        messages: Vec<TargetedMessage<M, N>>,
    ) -> Self {
        Step {
            output,
            fault_log,
            messages,
        }
    }

    /// Returns the same step, with the given additional output.
    pub fn with_output<T: Into<Option<O>>>(mut self, output: T) -> Self {
        self.output.extend(output.into());
        self
    }

    /// Converts `self` into a step of another type, given conversion methods for output and
    /// messages.
    pub fn map<M2, O2, FO, FM>(self, f_out: FO, f_msg: FM) -> Step<M2, O2, N>
    where
        FO: Fn(O) -> O2,
        FM: Fn(M) -> M2,
    {
        Step {
            output: self.output.into_iter().map(f_out).collect(),
            fault_log: self.fault_log,
            messages: self.messages.into_iter().map(|tm| tm.map(&f_msg)).collect(),
        }
    }

    /// Extends `self` with `other`s messages and fault logs, and returns `other.output`.
    pub fn extend_with<M2, O2, FM>(&mut self, other: Step<M2, O2, N>, f_msg: FM) -> Vec<O2>
    where
        FM: Fn(M2) -> M,
    {
        self.fault_log.extend(other.fault_log);
        let msgs = other.messages.into_iter().map(|tm| tm.map(&f_msg));
        self.messages.extend(msgs);
        other.output
    }

    /// Adds the outputs, fault logs and messages of `other` to `self`.
    pub fn extend(&mut self, other: Self) {
        self.output.extend(other.output);
        self.fault_log.extend(other.fault_log);
        self.messages.extend(other.messages);
    }

    /// Extends this step with `other` and returns the result.
    pub fn join(mut self, other: Self) -> Self {
        self.extend(other);
        self
    }

    /// Returns `true` if there are no messages, faults or outputs.
    pub fn is_empty(&self) -> bool {
        self.output.is_empty() && self.fault_log.is_empty() && self.messages.is_empty()
    }
}

impl<M, O, N> From<FaultLog<N>> for Step<M, O, N> {
    fn from(fault_log: FaultLog<N>) -> Self {
        Step {
            fault_log,
            ..Step::default()
        }
    }
}

impl<M, O, N> From<Fault<N>> for Step<M, O, N> {
    fn from(fault: Fault<N>) -> Self {
        Step {
            fault_log: fault.into(),
            ..Step::default()
        }
    }
}

impl<M, O, N> From<TargetedMessage<M, N>> for Step<M, O, N> {
    fn from(msg: TargetedMessage<M, N>) -> Self {
        Step {
            messages: once(msg).collect(),
            ..Step::default()
        }
    }
}

impl<I, M, O, N> From<I> for Step<M, O, N>
where
    I: IntoIterator<Item = TargetedMessage<M, N>>,
{
    fn from(msgs: I) -> Self {
        Step {
            messages: msgs.into_iter().collect(),
            ..Step::default()
        }
    }
}

/// An interface to objects with epoch numbers. Different algorithms may have different internal
/// notion of _epoch_. This interface summarizes the properties that are essential for the message
/// sender queue.
pub trait Epoched {
    /// Type of epoch.
    type Epoch: EpochT;

    /// Returns the object's epoch number.
    fn epoch(&self) -> Self::Epoch;
}

/// An alias for the type of `Step` returned by `D`'s methods.
pub type DaStep<D> =
    Step<<D as DistAlgorithm>::Message, <D as DistAlgorithm>::Output, <D as DistAlgorithm>::NodeId>;

impl<'i, M, O, N> Step<M, O, N>
where
    N: NodeIdT,
    M: 'i + Clone + SenderQueueableMessage,
{
    /// Removes and returns any messages that are not yet accepted by remote nodes according to the
    /// mapping `remote_epochs`. This way the returned messages are postponed until later, and the
    /// remaining messages can be sent to remote nodes without delay.
    pub fn defer_messages(
        &mut self,
        peer_epochs: &BTreeMap<N, M::Epoch>,
        max_future_epochs: u64,
    ) -> Vec<(N, M)> {
        let mut deferred_msgs: Vec<(N, M)> = Vec::new();
        let mut passed_msgs: Vec<_> = Vec::new();
        for msg in self.messages.drain(..) {
            match msg.target.clone() {
                Target::Node(id) => {
                    if let Some(&them) = peer_epochs.get(&id) {
                        if msg.message.is_premature(them, max_future_epochs) {
                            deferred_msgs.push((id, msg.message));
                        } else if !msg.message.is_obsolete(them) {
                            passed_msgs.push(msg);
                        }
                    }
                }
                Target::All => {
                    if peer_epochs
                        .values()
                        .all(|&them| msg.message.is_accepted(them, max_future_epochs))
                    {
                        passed_msgs.push(msg);
                    } else {
                        // The `Target::All` message is split into two sets of point messages: those
                        // which can be sent without delay and those which should be postponed.
                        for (id, &them) in peer_epochs {
                            if msg.message.is_premature(them, max_future_epochs) {
                                deferred_msgs.push((id.clone(), msg.message.clone()));
                            } else if !msg.message.is_obsolete(them) {
                                passed_msgs
                                    .push(Target::Node(id.clone()).message(msg.message.clone()));
                            }
                        }
                    }
                }
            }
        }
        self.messages.extend(passed_msgs);
        deferred_msgs
    }
}

/// A distributed algorithm that defines a message flow.
pub trait DistAlgorithm: Send + Sync {
    /// Unique node identifier.
    type NodeId: NodeIdT;
    /// The input provided by the user.
    type Input;
    /// The output type. Some algorithms return an output exactly once, others return multiple
    /// times.
    type Output;
    /// The messages that need to be exchanged between the instances in the participating nodes.
    type Message: Message;
    /// The errors that can occur during execution.
    type Error: Fail;

    /// Handles an input provided by the user, and returns
    fn handle_input(&mut self, input: Self::Input) -> Result<DaStep<Self>, Self::Error>
    where
        Self: Sized;

    /// Handles a message received from node `sender_id`.
    fn handle_message(
        &mut self,
        sender_id: &Self::NodeId,
        message: Self::Message,
    ) -> Result<DaStep<Self>, Self::Error>
    where
        Self: Sized;

    /// Returns `true` if execution has completed and this instance can be dropped.
    fn terminated(&self) -> bool;

    /// Returns this node's own ID.
    fn our_id(&self) -> &Self::NodeId;
}
