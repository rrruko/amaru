#![allow(clippy::wildcard_enum_match_arm, clippy::unwrap_used, clippy::panic)]

use crate::{
    cast_msg,
    effect::{StageEffect, StageResponse},
    BoxFuture, Effects, Instant, Message, Name, StageBuildRef, StageGraph, StageRef, State,
};
use std::{
    any::Any,
    collections::{HashMap, VecDeque},
    future::{poll_fn, Future},
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::Poll,
    time::Duration,
};
use tokio::sync::mpsc::unbounded_channel;

pub use receiver::Receiver;
pub use running::{Blocked, SimulationRunning};

use either::Either;
use parking_lot::Mutex;
use state::{InitStageData, InitStageState, StageData, StageState, Transition};

mod receiver;
mod running;
mod state;

pub(crate) type EffectBox =
    Arc<Mutex<Option<Either<StageEffect<Box<dyn Message>>, StageResponse>>>>;

pub(crate) fn airlock_effect<Out>(
    eb: &EffectBox,
    effect: StageEffect<Box<dyn Message>>,
    mut response: impl FnMut(Option<StageResponse>) -> Option<Out> + Send + 'static,
) -> BoxFuture<'static, Out> {
    let eb = eb.clone();
    let mut effect = Some(effect);
    Box::pin(poll_fn(move |_| {
        let mut eb = eb.lock();
        if let Some(effect) = effect.take() {
            match eb.take() {
                Some(Either::Left(x)) => panic!("effect already set: {:?}", x),
                // it is either Some(Right(Unit)) after Receive or None otherwise
                Some(Either::Right(StageResponse::Unit)) | None => {}
                Some(Either::Right(resp)) => {
                    panic!("effect airlock contains leftover response: {:?}", resp)
                }
            }
            *eb = Some(Either::Left(effect));
            Poll::Pending
        } else {
            let Some(out) = eb.take() else {
                return Poll::Pending;
            };
            let out = match out {
                Either::Left(x) => panic!("expected response, got effect: {:?}", x),
                Either::Right(x) => response(Some(x)),
            };
            out.map(Poll::Ready).unwrap_or(Poll::Pending)
        }
    }))
}

/// A fully controllable and deterministic [`StageGraph`] for testing purposes.
///
/// Execution is controlled entirely via the [`SimulationRunning`] handle returned from
/// [`StageGraph::run`].
///
/// The general principle is that each stage is suspended whenever it needs new
/// input (even when there is a message available in the mailbox) or when it uses
/// any of the effects provided (like [`StageRef::send`] or [`Interrupter::interrupt`]).
/// Resuming the given effect will not run the stage, but it will make it runnable
/// again when performing the next simulation step.
///
/// Example:
/// ```rust
/// use pure_stage::{StageGraph, simulation::SimulationBuilder, StageRef};
///
/// let mut network = SimulationBuilder::default();
/// let stage = network.stage(
///     "basic",
///     async |(mut state, out), msg: u32, eff| {
///         state += msg;
///         eff.send(&out, state).await;
///         Ok((state, out))
///     },
///     (1u32, StageRef::noop::<u32>()),
/// );
/// let (output, mut rx) = network.output("output");
/// let stage = network.wire_up(stage, |state| state.1 = output.without_state());
/// let mut running = network.run();
///
/// // first check that the stages start out suspended on Receive
/// running.try_effect().unwrap_err().assert_idle();
///
/// // then insert some input and check reaction
/// running.enqueue_msg(&stage, [1]);
/// running.resume_receive(&stage).unwrap();
/// running.effect().assert_send(&stage, &output, 2u32);
/// running.resume_send(&stage, &output, 2u32).unwrap();
/// running.effect().assert_receive(&stage);
///
/// running.resume_receive(&output).unwrap();
/// running.effect().assert_receive(&output);
///
/// assert_eq!(rx.drain().collect::<Vec<_>>(), vec![2]);
/// ```
pub struct SimulationBuilder {
    stages: HashMap<Name, InitStageData>,
    effect: EffectBox,
    clock: Arc<AtomicU64>,
    now: Arc<dyn Fn() -> Instant + Send + Sync>,
    mailbox_size: usize,
}

impl SimulationBuilder {
    pub fn with_mailbox_size(mut self, size: usize) -> Self {
        self.mailbox_size = size;
        self
    }
}

impl Default for SimulationBuilder {
    fn default() -> Self {
        let clock_base = tokio::time::Instant::now();
        let clock = Arc::new(AtomicU64::new(0));
        let clock2 = clock.clone();
        let now = Arc::new(move || {
            Instant::from_tokio(clock_base + Duration::from_nanos(clock2.load(Ordering::Relaxed)))
        });

        Self {
            stages: Default::default(),
            effect: Default::default(),
            clock,
            now,
            mailbox_size: 10,
        }
    }
}

impl SimulationBuilder {
    /// Construct a stage that sends received messages to a [`Receiver`] that is also returned.
    ///
    /// Note that you can control the forwarding of these messages by delaying the resumption of
    /// the [`Effect::Receive`] if you run in single-step mode. Otherwise, it is also possible
    /// to have this stage interrupt the simulation when an incoming message fits a given predicate,
    /// see [`output_interrupt`](Self::output_interrupt).
    pub fn output<T: Message>(&mut self, name: impl AsRef<str>) -> (StageRef<T, ()>, Receiver<T>) {
        let (tx, rx) = unbounded_channel();
        let stage = self.stage(
            &name,
            move |_st, msg, _eff| {
                let tx = tx.clone();
                async move { tx.send(msg).map_err(|_| anyhow::anyhow!("channel closed")) }
            },
            (),
        );
        (self.wire_up(stage, |_| {}), Receiver::new(rx))
    }

    /// Construct a stage that sends received messages to a [`Receiver`] that is also returned.
    ///
    /// Note that you can control the forwarding of these messages by delaying the resumption of
    /// the [`Effect::Receive`] if you run in single-step mode. Otherwise, it is also possible
    /// to have this stage interrupt the simulation when an incoming message fits a given predicate.
    ///
    /// Example:
    /// ```rust
    /// # use pure_stage::{StageGraph, simulation::SimulationBuilder};
    /// let mut network = SimulationBuilder::default();
    /// let (output, mut rx) = network.output_interrupt("output", |msg| *msg == 2);
    /// let mut running = network.run();
    ///
    /// running.enqueue_msg(&output, [1, 2, 3]);
    ///
    /// running.run_until_blocked().assert_interrupted("output");
    /// assert_eq!(rx.drain().collect::<Vec<_>>(), vec![1]);
    ///
    /// running.resume_interrupt(&output);
    /// assert_eq!(rx.drain().collect::<Vec<_>>(), vec![]);
    ///
    /// running.run_until_blocked().assert_idle();
    /// assert_eq!(rx.drain().collect::<Vec<_>>(), vec![2, 3]);
    /// ```
    pub fn output_interrupt<T: Message>(
        &mut self,
        name: impl AsRef<str>,
        interrupt: impl Fn(&T) -> bool + Send + 'static,
    ) -> (StageRef<T, ()>, Receiver<T>) {
        let (tx, rx) = unbounded_channel();
        let stage = self.stage(
            &name,
            move |_st, msg, eff| {
                let tx = tx.clone();
                let interrupt = interrupt(&msg);
                async move {
                    if interrupt {
                        eff.interrupt().await;
                    }
                    tx.send(msg).map_err(|_| anyhow::anyhow!("channel closed"))
                }
            },
            (),
        );
        let stage = self.wire_up(stage, |_| {});
        (stage, Receiver::new(rx))
    }
}

impl super::StageGraph for SimulationBuilder {
    type Running = SimulationRunning;
    type RefAux<Msg, State> = ();

    fn stage<Msg: Message, St: State, F, Fut>(
        &mut self,
        name: impl AsRef<str>,
        mut f: F,
        state: St,
    ) -> StageBuildRef<Msg, St, Self::RefAux<Msg, St>>
    where
        F: FnMut(St, Msg, Effects<Msg, St>) -> Fut + 'static + Send,
        Fut: Future<Output = anyhow::Result<St>> + 'static + Send,
    {
        let name = Name::from(name.as_ref());
        let me = StageRef {
            name: name.clone(),
            _ph: PhantomData,
        };
        let effects = Effects::new(me, self.effect.clone(), self.now.clone());
        let transition: Transition =
            Box::new(move |state: Box<dyn State>, msg: Box<dyn Message>| {
                let state = (state as Box<dyn Any>).downcast::<St>().unwrap();
                let msg = cast_msg::<Msg>(msg).unwrap();
                let state = f(*state, msg, effects.clone());
                Box::pin(async move { Ok(Box::new(state.await?) as Box<dyn State>) })
            });

        if let Some(old) = self.stages.insert(
            name.clone(),
            InitStageData {
                state: InitStageState::Uninitialized,
                mailbox: VecDeque::new(),
                transition,
            },
        ) {
            panic!("stage {name} already exists with state {:?}", old.state);
        }

        StageBuildRef {
            name,
            state,
            network: (),
            _ph: PhantomData,
        }
    }

    fn wire_up<Msg: Message, St: State>(
        &mut self,
        stage: crate::StageBuildRef<Msg, St, Self::RefAux<Msg, St>>,
        f: impl FnOnce(&mut St),
    ) -> StageRef<Msg, St> {
        let StageBuildRef {
            name,
            mut state,
            network: (),
            _ph,
        } = stage;

        f(&mut state);
        let data = self.stages.get_mut(&name).unwrap();
        data.state = InitStageState::Idle(Box::new(state));

        StageRef {
            name,
            _ph: PhantomData,
        }
    }

    fn run(self) -> Self::Running {
        let Self {
            stages: s,
            effect,
            clock,
            now,
            mailbox_size,
        } = self;
        let mut stages = HashMap::new();
        for (
            name,
            InitStageData {
                mailbox,
                state,
                transition,
            },
        ) in s
        {
            let state = match state {
                InitStageState::Uninitialized => panic!("forgot to wire up stage `{name}`"),
                InitStageState::Idle(state) => StageState::Idle(state),
            };
            let data = StageData {
                name: name.clone(),
                mailbox,
                state,
                transition,
                waiting: Some(StageEffect::Receive),
                senders: VecDeque::new(),
            };
            stages.insert(name, data);
        }
        SimulationRunning::new(stages, effect, clock, now, mailbox_size)
    }
}
