use std::{marker::PhantomData, time::Duration};

use derive_where::derive_where;

use crate::event::{ScheduleEvent, ActiveTimer};

#[derive_where(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Timer<M> {
    id: Option<ActiveTimer>,
    period: Duration,
    _m: PhantomData<M>,
}

impl<M> Timer<M> {
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            id: None,
            _m: PhantomData,
        }
    }

    // TODO support ScheduleEventFor
    pub fn set(&mut self, event: M, context: &mut impl ScheduleEvent<M>) -> anyhow::Result<()>
    where
        M: Clone + Send + 'static,
    {
        let replaced = self.id.replace(context.set(self.period, event)?);
        anyhow::ensure!(replaced.is_none());
        Ok(())
    }

    pub fn unset(&mut self, context: &mut impl ScheduleEvent<M>) -> anyhow::Result<()> {
        context.unset(
            self.id
                .take()
                .ok_or(anyhow::format_err!("missing timer id"))?,
        )
    }

    pub fn ensure_set(
        &mut self,
        event: M,
        context: &mut impl ScheduleEvent<M>,
    ) -> anyhow::Result<()>
    where
        M: Clone + Send + 'static,
    {
        if self.id.is_none() {
            self.set(event, context)?
        }
        Ok(())
    }

    pub fn ensure_unset(&mut self, context: &mut impl ScheduleEvent<M>) -> anyhow::Result<()> {
        if self.id.is_some() {
            self.unset(context)?
        }
        Ok(())
    }
}
