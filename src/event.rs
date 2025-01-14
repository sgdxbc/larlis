use std::{any::type_name, fmt::Debug, marker::PhantomData, time::Duration};

use derive_more::{Deref, DerefMut, Display, Error};
use derive_where::derive_where;

pub mod combinators;
pub mod task;

pub trait SendEvent<M> {
    fn send(&mut self, event: M) -> anyhow::Result<()>;
}

impl<E: SendEvent<M>, M> SendEvent<M> for &mut E {
    fn send(&mut self, event: M) -> anyhow::Result<()> {
        E::send(self, event)
    }
}

pub trait OnEvent<C> {
    type Event;

    fn on_event(&mut self, event: Self::Event, context: &mut C) -> anyhow::Result<()>;
}

impl<S: OnEvent<C>, C> OnEvent<C> for &mut S {
    type Event = S::Event;

    fn on_event(&mut self, event: Self::Event, context: &mut C) -> anyhow::Result<()> {
        S::on_event(self, event, context)
    }
}

#[derive(Debug, Display, Error)]
pub struct Exit;

// the abstraction of *activated timer as a resource*, though leaky
// as long as an instance of ActiveTimer is around and owned by someone, the
// context that allocated the instance will keep scheduling the timer i.e.
// call OnEvent::on_event on someone
//
// (well in most sensible cases the owner "someone" and the callee "someone" is
// probably the same "one". some fact that neither enforced nor reflected in the
// types for now)
//
// the instances of ActiveTimer should never be dropped externally. instead pass
// them into ScheduleEvent::unset, complete the resource management circle. also
// ideally they should not be `Clone`, to prevent double free problems
//
// neither of above is enforced by types for now. the reason of impl `Clone` is
// for model checking: the ActiveTimer instances, as part of the checked system
// states, is preferred to be able to be simply `Clone`d. in another word, i
// don't want the ActiveTimer itself to be `Clone` but any state that contains
// it to be so. no such expressiveness in Rust as far as i know
//
// for the drop thing i just feel lazy to code for it, maybe later
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActiveTimer(pub u32);

pub trait ScheduleEvent<M> {
    // the actual "user facing" interface. `OnEvent` implementations should always play with this
    // one, since certain ScheduleEvent implementations (e.g. search timer state) do not support
    // `set_internal`
    // the (currently) sole reason for `set_internal` to be part of ScheduleEvent is that the below
    // adaption of ScheduleEvent impl on Erase<...> can only be performed on `set_internal` layer
    fn set(&mut self, period: Duration, event: M) -> anyhow::Result<ActiveTimer>
    where
        M: Send + Clone + 'static,
    {
        self.set_internal(period, move || event.clone())
    }

    #[allow(unused)]
    fn set_internal(
        &mut self,
        period: Duration,
        event: impl FnMut() -> M + Send + 'static,
    ) -> anyhow::Result<ActiveTimer> {
        anyhow::bail!("unimplemented")
    }

    fn unset(&mut self, id: ActiveTimer) -> anyhow::Result<()>;
}

impl<T: ScheduleEvent<M>, M> ScheduleEvent<M> for &mut T {
    fn set(&mut self, period: Duration, event: M) -> anyhow::Result<ActiveTimer>
    where
        M: Clone + Send + 'static,
    {
        T::set(self, period, event)
    }

    fn set_internal(
        &mut self,
        period: Duration,
        event: impl FnMut() -> M + Send + 'static,
    ) -> anyhow::Result<ActiveTimer> {
        T::set_internal(self, period, event)
    }

    fn unset(&mut self, id: ActiveTimer) -> anyhow::Result<()> {
        T::unset(self, id)
    }
}

#[derive_where(Debug, Clone; S)]
#[derive(Deref, DerefMut)]
pub struct Untyped<C, S>(
    #[deref]
    #[deref_mut]
    S,
    PhantomData<C>,
);

impl<C, S> Untyped<C, S> {
    pub fn new(state: S) -> Self {
        Self(state, Default::default())
    }
}

#[allow(clippy::type_complexity)]
pub struct UntypedEvent<S, C: ?Sized>(
    pub Box<dyn FnOnce(&mut S, &mut C) -> anyhow::Result<()> + Send>,
);

impl<S, C> Debug for UntypedEvent<S, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}(_)", type_name::<Self>())
    }
}

impl<S, C> OnEvent<C> for Untyped<C, S> {
    type Event = UntypedEvent<S, C>;

    fn on_event(
        &mut self,
        UntypedEvent(event): Self::Event,
        context: &mut C,
    ) -> anyhow::Result<()> {
        event(&mut self.0, context)
    }
}

pub trait OnErasedEvent<M, C: ?Sized> {
    fn on_event(&mut self, event: M, context: &mut C) -> anyhow::Result<()>;
}

#[derive_where(Debug, Clone, Default; E)]
#[derive(Deref, DerefMut)]
pub struct Erase<S, C: ?Sized, E>(
    #[deref]
    #[deref_mut]
    E,
    PhantomData<(S, C)>,
);

impl<S, C, E> Erase<S, C, E> {
    pub fn new(inner: E) -> Self {
        Self(inner, Default::default())
    }
}

// something i really want
//   type EraseOf<T> = type<S, C> Erase<S, C, T<S, C>>
// Rust does not have (real) higher rank types, either on parameter or on return
// position. the probably only way to simulate is through macros, but i don't
// think that worth
// so i will just repeat this type alias pattern for various `T`s everywhere in
// the codebase

impl<E: SendEvent<UntypedEvent<S, C>>, S: OnErasedEvent<M, C>, C: ?Sized, M: Send + 'static>
    SendEvent<M> for Erase<S, C, E>
{
    fn send(&mut self, event: M) -> anyhow::Result<()> {
        self.0.send(UntypedEvent(Box::new(move |state, context| {
            state.on_event(event, context)
        })))
    }
}

impl<
        T: ScheduleEvent<UntypedEvent<S, C>>,
        S: OnErasedEvent<M, C>,
        C,
        M: Clone + Send + 'static,
    > ScheduleEvent<M> for Erase<S, C, T>
{
    fn set_internal(
        &mut self,
        period: Duration,
        mut event: impl FnMut() -> M + Send + 'static,
    ) -> anyhow::Result<ActiveTimer> {
        self.0.set_internal(period, move || {
            let event = event();
            UntypedEvent(Box::new(move |state, context| {
                state.on_event(event, context)
            }))
        })
    }

    fn unset(&mut self, id: ActiveTimer) -> anyhow::Result<()> {
        self.0.unset(id)
    }
}

pub type Work<S, C> = Box<dyn FnOnce(&mut S, &mut C) -> anyhow::Result<()> + Send>;

pub trait Submit<S, C> {
    // the ergonomics here breaks some, so hold on it
    // fn submit(&mut self, work: impl Into<Work<S, C>>) -> anyhow::Result<()>;
    fn submit(&mut self, work: Work<S, C>) -> anyhow::Result<()>;
}

// impl<E: SendEvent<UntypedEvent<S, C>>, S, C> Submit<S, C> for E {
//     fn submit(&mut self, work: Work<S, C>) -> anyhow::Result<()> {
//         self.send(UntypedEvent(work))
//     }
// }

pub trait SendEventFor<S, C: ?Sized> {
    fn send<M: Send + 'static>(&mut self, event: M) -> anyhow::Result<()>
    where
        S: OnErasedEvent<M, C>;
}

impl<E: SendEvent<UntypedEvent<S, C>>, S, C: ?Sized> SendEventFor<S, C> for Erase<S, C, E> {
    fn send<M: Send + 'static>(&mut self, event: M) -> anyhow::Result<()>
    where
        S: OnErasedEvent<M, C>,
    {
        SendEvent::send(self, event)
    }
}

pub trait ScheduleEventFor<S, C> {
    fn set<M: Clone + Send + 'static>(
        &mut self,
        period: Duration,
        event: M,
    ) -> anyhow::Result<ActiveTimer>
    where
        S: OnErasedEvent<M, C>;

    fn unset(&mut self, id: ActiveTimer) -> anyhow::Result<()>;
}

impl<T: ScheduleEvent<UntypedEvent<S, C>>, S, C> ScheduleEventFor<S, C> for Erase<S, C, T> {
    fn set<M: Clone + Send + 'static>(
        &mut self,
        period: Duration,
        event: M,
    ) -> anyhow::Result<ActiveTimer>
    where
        S: OnErasedEvent<M, C>,
    {
        ScheduleEvent::set(self, period, event)
    }

    fn unset(&mut self, id: ActiveTimer) -> anyhow::Result<()> {
        // cannot just forward from `self`, because that `ScheduleEvent` is bounded on
        // `S: OnErasedEvent<..>` as a whole, though that is unnecessary for `unset`
        // consider switch to opposite, implement `set` and `unset` here and forward to there
        ScheduleEvent::unset(&mut self.0, id)
    }
}
