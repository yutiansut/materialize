// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logic for selecting timestamps for various operations on collections.

use std::fmt;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use differential_dataflow::lattice::Lattice;
use mz_compute_types::ComputeInstanceId;
use mz_expr::MirScalarExpr;
use mz_ore::cast::CastLossy;
use mz_repr::explain::ExprHumanizer;
use mz_repr::{GlobalId, RowArena, ScalarType, Timestamp, TimestampManipulation};
use mz_sql::plan::QueryWhen;
use mz_sql::session::vars::IsolationLevel;
use mz_storage_types::sources::Timeline;
use serde::{Deserialize, Serialize};
use timely::progress::frontier::AntichainRef;
use timely::progress::{Antichain, Timestamp as TimelyTimestamp};
use tracing::{event, Level};

use crate::catalog::CatalogState;
use crate::coord::id_bundle::CollectionIdBundle;
use crate::coord::timeline::TimelineContext;
use crate::coord::Coordinator;
use crate::optimize::dataflows::{prep_scalar_expr, ExprPrepStyle};
use crate::session::Session;
use crate::AdapterError;

/// The timeline and timestamp context of a read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TimestampContext<T> {
    /// Read is executed in a specific timeline with a specific timestamp.
    TimelineTimestamp {
        timeline: Timeline,
        /// The timestamp that was chosen for a read. This can differ from the
        /// `oracle_ts` when collections are not readable at the (linearized)
        /// timestamp for the oracle. In those cases (when the chosen timestamp
        /// is further ahead than the oracle timestamp) we have to delay
        /// returning peek results until the timestamp oracle is also
        /// sufficiently advanced.
        chosen_ts: T,
        /// The timestamp that would have been chosen for the read by the
        /// (linearized) timestamp oracle). In most cases this will be picked as
        /// the `chosen_ts`.
        oracle_ts: Option<T>,
    },
    /// Read is execute without a timeline or timestamp.
    NoTimestamp,
}

impl<T: TimestampManipulation> TimestampContext<T> {
    /// Creates a `TimestampContext` from a timestamp and `TimelineContext`.
    pub fn from_timeline_context(
        chosen_ts: T,
        oracle_ts: Option<T>,
        transaction_timeline: Option<Timeline>,
        timeline_context: &TimelineContext,
    ) -> TimestampContext<T> {
        match timeline_context {
            TimelineContext::TimelineDependent(timeline) => {
                if let Some(transaction_timeline) = transaction_timeline {
                    assert_eq!(timeline, &transaction_timeline);
                }
                Self::TimelineTimestamp {
                    timeline: timeline.clone(),
                    chosen_ts,
                    oracle_ts,
                }
            }
            TimelineContext::TimestampDependent => {
                // We default to the `Timeline::EpochMilliseconds` timeline if one doesn't exist.
                Self::TimelineTimestamp {
                    timeline: transaction_timeline.unwrap_or(Timeline::EpochMilliseconds),
                    chosen_ts,
                    oracle_ts,
                }
            }
            TimelineContext::TimestampIndependent => Self::NoTimestamp,
        }
    }

    /// The timeline belonging to this context, if one exists.
    pub fn timeline(&self) -> Option<&Timeline> {
        self.timeline_timestamp().map(|tt| tt.0)
    }

    /// The timestamp belonging to this context, if one exists.
    pub fn timestamp(&self) -> Option<&T> {
        self.timeline_timestamp().map(|tt| tt.1)
    }

    /// The timeline and timestamp belonging to this context, if one exists.
    pub fn timeline_timestamp(&self) -> Option<(&Timeline, &T)> {
        match self {
            Self::TimelineTimestamp {
                timeline,
                chosen_ts,
                ..
            } => Some((timeline, chosen_ts)),
            Self::NoTimestamp => None,
        }
    }

    /// The timestamp belonging to this context, or a sensible default if one does not exists.
    pub fn timestamp_or_default(&self) -> T {
        match self {
            Self::TimelineTimestamp { chosen_ts, .. } => chosen_ts.clone(),
            // Anything without a timestamp is given the maximum possible timestamp to indicate
            // that they have been closed up until the end of time. This allows us to SUBSCRIBE to
            // static views.
            Self::NoTimestamp => T::maximum(),
        }
    }

    /// Whether or not the context contains a timestamp.
    pub fn contains_timestamp(&self) -> bool {
        self.timestamp().is_some()
    }

    /// Converts this `TimestampContext` to an `Antichain`.
    pub fn antichain(&self) -> Antichain<T> {
        Antichain::from_elem(self.timestamp_or_default())
    }
}

#[async_trait(?Send)]
impl TimestampProvider for Coordinator {
    /// Reports a collection's current read frontier.
    fn compute_read_frontier<'a>(
        &'a self,
        instance: ComputeInstanceId,
        id: GlobalId,
    ) -> AntichainRef<'a, Timestamp> {
        self.controller
            .compute
            .collection(instance, id)
            .expect("id does not exist")
            .read_frontier()
    }

    /// Reports a collection's current read capability.
    fn compute_read_capability<'a>(
        &'a self,
        instance: ComputeInstanceId,
        id: GlobalId,
    ) -> &'a Antichain<Timestamp> {
        self.controller
            .compute
            .collection(instance, id)
            .expect("id does not exist")
            .read_capability()
    }

    /// Reports a collection's current write frontier.
    fn compute_write_frontier<'a>(
        &'a self,
        instance: ComputeInstanceId,
        id: GlobalId,
    ) -> AntichainRef<'a, Timestamp> {
        self.controller
            .compute
            .collection(instance, id)
            .expect("id does not exist")
            .write_frontier()
    }

    /// Accumulation of read capabilities for the collection.
    fn storage_read_capabilities<'a>(&'a self, id: GlobalId) -> AntichainRef<'a, Timestamp> {
        self.controller
            .storage
            .collection(id)
            .expect("id does not exist")
            .read_capabilities
            .frontier()
    }

    /// The implicit capability associated with collection creation.
    fn storage_implied_capability<'a>(&'a self, id: GlobalId) -> &'a Antichain<Timestamp> {
        &self
            .controller
            .storage
            .collection(id)
            .expect("id does not exist")
            .implied_capability
    }

    /// Reported write frontier.
    fn storage_write_frontier<'a>(&'a self, id: GlobalId) -> &'a Antichain<Timestamp> {
        &self
            .controller
            .storage
            .collection(id)
            .expect("id does not exist")
            .write_frontier
    }
}

#[async_trait(?Send)]
pub trait TimestampProvider {
    fn compute_read_frontier<'a>(
        &'a self,
        instance: ComputeInstanceId,
        id: GlobalId,
    ) -> AntichainRef<'a, Timestamp>;
    fn compute_read_capability<'a>(
        &'a self,
        instance: ComputeInstanceId,
        id: GlobalId,
    ) -> &'a Antichain<Timestamp>;
    fn compute_write_frontier<'a>(
        &'a self,
        instance: ComputeInstanceId,
        id: GlobalId,
    ) -> AntichainRef<'a, Timestamp>;

    fn storage_read_capabilities<'a>(&'a self, id: GlobalId) -> AntichainRef<'a, Timestamp>;
    fn storage_implied_capability<'a>(&'a self, id: GlobalId) -> &'a Antichain<Timestamp>;
    fn storage_write_frontier<'a>(&'a self, id: GlobalId) -> &'a Antichain<Timestamp>;

    fn get_timeline(timeline_context: &TimelineContext) -> Option<Timeline> {
        let timeline = match timeline_context {
            TimelineContext::TimelineDependent(timeline) => Some(timeline.clone()),
            // We default to the `Timeline::EpochMilliseconds` timeline if one doesn't exist.
            TimelineContext::TimestampDependent => Some(Timeline::EpochMilliseconds),
            TimelineContext::TimestampIndependent => None,
        };

        timeline
    }

    /// Returns a `Timeline` whose timestamp oracle we have to use to get a
    /// linearized read timestamp, _iff_ linearization is needed.
    fn get_linearized_timeline(
        isolation_level: &IsolationLevel,
        when: &QueryWhen,
        timeline_context: &TimelineContext,
    ) -> Option<Timeline> {
        let timeline = Self::get_timeline(timeline_context);

        // In order to use a timestamp oracle, we must be in the context of some timeline. In that
        // context we would use the timestamp oracle in the following scenarios:
        // - The isolation level is Strict Serializable and the `when` allows us to use the
        //   the timestamp oracle (ex: queries with no AS OF).
        // - The `when` requires us to use the timestamp oracle (ex: read-then-write queries).
        let linearized_timeline = match &timeline {
            Some(timeline)
                if when.must_advance_to_timeline_ts()
                    || (when.can_advance_to_timeline_ts()
                        && matches!(
                            isolation_level,
                            IsolationLevel::StrictSerializable
                                | IsolationLevel::StrongSessionSerializable
                        )) =>
            {
                Some(timeline.clone())
            }
            _ => None,
        };

        linearized_timeline
    }

    /// Determines the timestamp for a query.
    ///
    /// Timestamp determination may fail due to the restricted validity of
    /// traces. Each has a `since` and `upper` frontier, and are only valid
    /// after `since` and sure to be available not after `upper`.
    ///
    /// The timeline that `id_bundle` belongs to is also returned, if one exists.
    async fn determine_timestamp_for(
        &self,
        catalog: &CatalogState,
        session: &Session,
        id_bundle: &CollectionIdBundle,
        when: &QueryWhen,
        compute_instance: ComputeInstanceId,
        timeline_context: &TimelineContext,
        oracle_read_ts: Option<Timestamp>,
        real_time_recency_ts: Option<mz_repr::Timestamp>,
        isolation_level: &IsolationLevel,
    ) -> Result<TimestampDetermination<mz_repr::Timestamp>, AdapterError> {
        // Each involved trace has a validity interval `[since, upper)`.
        // The contents of a trace are only guaranteed to be correct when
        // accumulated at a time greater or equal to `since`, and they
        // are only guaranteed to be currently present for times not
        // greater or equal to `upper`.
        //
        // The plan is to first determine a timestamp, based on the requested
        // timestamp policy, and then determine if it can be satisfied using
        // the compacted arrangements we have at hand. It remains unresolved
        // what to do if it cannot be satisfied (perhaps the query should use
        // a larger timestamp and block, perhaps the user should intervene).

        let since = self.least_valid_read(id_bundle);
        let upper = self.least_valid_write(id_bundle);
        let largest_not_in_advance_of_upper = Coordinator::largest_not_in_advance_of_upper(&upper);

        let timeline = Self::get_timeline(timeline_context);
        let linearized_timeline =
            Self::get_linearized_timeline(isolation_level, when, timeline_context);
        // TODO: We currently split out getting the oracle timestamp because
        // it's a potentially expensive call, but a call that can be done in an
        // async task. TimestampProvider is not Send (nor Sync), so we cannot do
        // the call to `determine_timestamp_for` (including the oracle call) on
        // an async task. If/when TimestampProvider can become Send, we can fold
        // the call to the TimestampOracle back into this function.
        //
        // We assert here that the logic that determines the oracle timestamp
        // matches our expectations.
        if linearized_timeline.is_some() {
            assert!(
                oracle_read_ts.is_some(),
                "should get a timestamp from the oracle for linearized timeline {:?} but didn't",
                timeline
            );
        }

        // Initialize candidate to the minimum correct time.
        let mut candidate = Timestamp::minimum();

        if let Some(timestamp) = when.advance_to_timestamp() {
            let ts = Coordinator::evaluate_when(catalog, timestamp, session)?;
            candidate.join_assign(&ts);
        }

        if when.advance_to_since() {
            candidate.advance_by(since.borrow());
        }

        // If we've acquired a read timestamp from the timestamp oracle, use it
        // as the new lower bound for the candidate.
        // In Strong Session Serializable, we ignore the oracle timestamp for now, unless we need
        // to use it.
        if let Some(timestamp) = &oracle_read_ts {
            if isolation_level != &IsolationLevel::StrongSessionSerializable
                || when.must_advance_to_timeline_ts()
            {
                candidate.join_assign(timestamp);
            }
        }

        // We advance to the upper in the following scenarios:
        // - The isolation level is Serializable and the `when` allows us to advance to upper (ex:
        //   queries with no AS OF). We avoid using the upper in Strict Serializable to prevent
        //   reading source data that is being written to in the future.
        // - The isolation level is Strict Serializable but there is no timelines and the `when`
        //   allows us to advance to upper.
        if when.can_advance_to_upper()
            && (isolation_level == &IsolationLevel::Serializable || timeline.is_none())
        {
            candidate.join_assign(&largest_not_in_advance_of_upper);
        }

        if let Some(real_time_recency_ts) = real_time_recency_ts {
            assert!(
                session.vars().real_time_recency()
                    && isolation_level == &IsolationLevel::StrictSerializable,
                "real time recency timestamp should only be supplied when real time recency \
                            is enabled and the isolation level is strict serializable"
            );
            candidate.join_assign(&real_time_recency_ts);
        }

        let mut session_oracle_read_ts = None;
        if isolation_level == &IsolationLevel::StrongSessionSerializable {
            if let Some(timeline) = &timeline {
                if let Some(oracle) = session.get_timestamp_oracle(timeline) {
                    let session_ts = oracle.read_ts();
                    candidate.join_assign(&session_ts);
                    session_oracle_read_ts = Some(session_ts);
                }
            }

            // When advancing the read timestamp under Strong Session Serializable, there is a
            // trade-off to make between freshness and latency. We can choose a timestamp close the
            // `upper`, but then later queries might block if the `upper` is too far into the
            // future. We can chose a timestamp close to the current time, but then we may not be
            // getting results that are as fresh as possible. As a heuristic, we choose the minimum
            // of now and the upper, where we use the global timestamp oracle read timestamp as a
            // proxy for now. If upper > now, then we choose now and prevent blocking future
            // queries. If upper < now, then we choose the upper and prevent blocking the current
            // query.
            if when.can_advance_to_upper() && when.can_advance_to_timeline_ts() {
                let mut advance_to = largest_not_in_advance_of_upper;
                if let Some(oracle_read_ts) = oracle_read_ts {
                    advance_to = std::cmp::min(advance_to, oracle_read_ts);
                }
                candidate.join_assign(&advance_to);
            }
        }

        // If the timestamp is greater or equal to some element in `since` we are
        // assured that the answer will be correct.
        //
        // It's ok for this timestamp to be larger than the current timestamp of
        // the timestamp oracle. For Strict Serializable queries, the Coord will
        // linearize the query by holding back the result until the timestamp
        // oracle catches up.
        let timestamp = if since.less_equal(&candidate) {
            event!(
                Level::DEBUG,
                conn_id = format!("{}", session.conn_id()),
                since = format!("{since:?}"),
                largest_not_in_advance_of_upper = format!("{largest_not_in_advance_of_upper}"),
                timestamp = format!("{candidate}")
            );
            candidate
        } else {
            coord_bail!(self.generate_timestamp_not_valid_error_msg(
                id_bundle,
                compute_instance,
                candidate
            ));
        };

        let timestamp_context = TimestampContext::from_timeline_context(
            timestamp,
            oracle_read_ts,
            timeline,
            timeline_context,
        );

        Ok(TimestampDetermination {
            timestamp_context,
            since,
            upper,
            largest_not_in_advance_of_upper,
            oracle_read_ts,
            session_oracle_read_ts,
        })
    }

    /// The smallest common valid read frontier among the specified collections.
    fn least_valid_read(&self, id_bundle: &CollectionIdBundle) -> Antichain<mz_repr::Timestamp> {
        let mut since = Antichain::from_elem(Timestamp::minimum());
        {
            for id in id_bundle.storage_ids.iter() {
                since.join_assign(self.storage_implied_capability(*id))
            }
        }
        {
            for (instance, compute_ids) in &id_bundle.compute_ids {
                for id in compute_ids.iter() {
                    since.join_assign(self.compute_read_capability(*instance, *id))
                }
            }
        }
        since
    }

    /// The smallest common valid write frontier among the specified collections.
    ///
    /// Times that are not greater or equal to this frontier are complete for all collections
    /// identified as arguments.
    fn least_valid_write(&self, id_bundle: &CollectionIdBundle) -> Antichain<mz_repr::Timestamp> {
        let mut since = Antichain::new();
        {
            for id in id_bundle.storage_ids.iter() {
                since.extend(self.storage_write_frontier(*id).iter().cloned());
            }
        }
        {
            for (instance, compute_ids) in &id_bundle.compute_ids {
                for id in compute_ids.iter() {
                    since.extend(self.compute_write_frontier(*instance, *id).iter().cloned());
                }
            }
        }
        since
    }

    fn generate_timestamp_not_valid_error_msg(
        &self,
        id_bundle: &CollectionIdBundle,
        compute_instance: ComputeInstanceId,
        candidate: mz_repr::Timestamp,
    ) -> String {
        let invalid_indexes =
            if let Some(compute_ids) = id_bundle.compute_ids.get(&compute_instance) {
                compute_ids
                    .iter()
                    .filter_map(|id| {
                        let since = self.compute_read_frontier(compute_instance, *id).to_owned();
                        if since.less_equal(&candidate) {
                            None
                        } else {
                            Some(since)
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
        let invalid_sources = id_bundle.storage_ids.iter().filter_map(|id| {
            let since = self.storage_read_capabilities(*id).to_owned();
            if since.less_equal(&candidate) {
                None
            } else {
                Some(since)
            }
        });
        let invalid = invalid_indexes
            .into_iter()
            .chain(invalid_sources)
            .collect::<Vec<_>>();
        format!(
            "Timestamp ({}) is not valid for all inputs: {:?}",
            candidate, invalid,
        )
    }
}

impl Coordinator {
    pub(crate) async fn oracle_read_ts(
        &self,
        session: &Session,
        timeline_ctx: &TimelineContext,
        when: &QueryWhen,
    ) -> Option<Timestamp> {
        let isolation_level = session.vars().transaction_isolation().clone();
        let linearized_timeline =
            Coordinator::get_linearized_timeline(&isolation_level, when, timeline_ctx);
        let oracle_read_ts = match linearized_timeline {
            Some(timeline) => {
                let timestamp_oracle = self.get_timestamp_oracle(&timeline);
                Some(timestamp_oracle.read_ts().await)
            }
            None => None,
        };

        oracle_read_ts
    }

    /// Determines the timestamp for a query.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn determine_timestamp(
        &self,
        session: &Session,
        id_bundle: &CollectionIdBundle,
        when: &QueryWhen,
        compute_instance: ComputeInstanceId,
        timeline_context: &TimelineContext,
        oracle_read_ts: Option<Timestamp>,
        real_time_recency_ts: Option<mz_repr::Timestamp>,
    ) -> Result<TimestampDetermination<mz_repr::Timestamp>, AdapterError> {
        let isolation_level = session.vars().transaction_isolation();
        let det = self
            .determine_timestamp_for(
                self.catalog().state(),
                session,
                id_bundle,
                when,
                compute_instance,
                timeline_context,
                oracle_read_ts,
                real_time_recency_ts,
                isolation_level,
            )
            .await?;
        self.metrics
            .determine_timestamp
            .with_label_values(&[
                match det.respond_immediately() {
                    true => "true",
                    false => "false",
                },
                isolation_level.as_str(),
                &compute_instance.to_string(),
            ])
            .inc();
        if !det.respond_immediately()
            && isolation_level == &IsolationLevel::StrictSerializable
            && real_time_recency_ts.is_none()
        {
            if let Some(strict) = det.timestamp_context.timestamp() {
                let serializable_det = self
                    .determine_timestamp_for(
                        self.catalog().state(),
                        session,
                        id_bundle,
                        when,
                        compute_instance,
                        timeline_context,
                        oracle_read_ts,
                        real_time_recency_ts,
                        &IsolationLevel::Serializable,
                    )
                    .await?;
                if let Some(serializable) = serializable_det.timestamp_context.timestamp() {
                    self.metrics
                        .timestamp_difference_for_strict_serializable_ms
                        .with_label_values(&[&compute_instance.to_string()])
                        .observe(f64::cast_lossy(u64::from(
                            strict.saturating_sub(*serializable),
                        )));
                }
            }
        }
        Ok(det)
    }

    /// The largest element not in advance of any object in the collection.
    ///
    /// Times that are not greater to this frontier are complete for all collections
    /// identified as arguments.
    pub(crate) fn largest_not_in_advance_of_upper(
        upper: &Antichain<mz_repr::Timestamp>,
    ) -> mz_repr::Timestamp {
        // We peek at the largest element not in advance of `upper`, which
        // involves a subtraction. If `upper` contains a zero timestamp there
        // is no "prior" answer, and we do not want to peek at it as it risks
        // hanging awaiting the response to data that may never arrive.
        if let Some(upper) = upper.as_option() {
            upper.step_back().unwrap_or_else(Timestamp::minimum)
        } else {
            // A complete trace can be read in its final form with this time.
            //
            // This should only happen for literals that have no sources or sources that
            // are known to have completed (non-tailed files for example).
            Timestamp::MAX
        }
    }

    pub(crate) fn evaluate_when(
        catalog: &CatalogState,
        mut timestamp: MirScalarExpr,
        session: &Session,
    ) -> Result<mz_repr::Timestamp, AdapterError> {
        let temp_storage = RowArena::new();
        prep_scalar_expr(&mut timestamp, ExprPrepStyle::AsOfUpTo)?;
        let evaled = timestamp.eval(&[], &temp_storage)?;
        if evaled.is_null() {
            coord_bail!("can't use {} as a mz_timestamp for AS OF or UP TO", evaled);
        }
        let ty = timestamp.typ(&[]);
        Ok(match ty.scalar_type {
            ScalarType::MzTimestamp => evaled.unwrap_mz_timestamp(),
            ScalarType::Numeric { .. } => {
                let n = evaled.unwrap_numeric().0;
                n.try_into()?
            }
            ScalarType::Int16 => i64::from(evaled.unwrap_int16()).try_into()?,
            ScalarType::Int32 => i64::from(evaled.unwrap_int32()).try_into()?,
            ScalarType::Int64 => evaled.unwrap_int64().try_into()?,
            ScalarType::UInt16 => u64::from(evaled.unwrap_uint16()).into(),
            ScalarType::UInt32 => u64::from(evaled.unwrap_uint32()).into(),
            ScalarType::UInt64 => evaled.unwrap_uint64().into(),
            ScalarType::TimestampTz { .. } => {
                evaled.unwrap_timestamptz().timestamp_millis().try_into()?
            }
            ScalarType::Timestamp { .. } => {
                evaled.unwrap_timestamp().timestamp_millis().try_into()?
            }
            _ => coord_bail!(
                "can't use {} as a mz_timestamp for AS OF or UP TO",
                catalog.for_session(session).humanize_column_type(&ty)
            ),
        })
    }
}

/// Information used when determining the timestamp for a query.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TimestampDetermination<T> {
    /// The chosen timestamp context from `determine_timestamp`.
    pub timestamp_context: TimestampContext<T>,
    /// The read frontier of all involved sources.
    pub since: Antichain<T>,
    /// The write frontier of all involved sources.
    pub upper: Antichain<T>,
    /// The largest timestamp not in advance of upper.
    pub largest_not_in_advance_of_upper: T,
    /// The value of the timeline's oracle timestamp, if used.
    pub oracle_read_ts: Option<T>,
    /// The value of the session local timestamp's oracle timestamp, if used.
    pub session_oracle_read_ts: Option<T>,
}

impl<T: TimestampManipulation> TimestampDetermination<T> {
    pub fn respond_immediately(&self) -> bool {
        match &self.timestamp_context {
            TimestampContext::TimelineTimestamp { chosen_ts, .. } => {
                !self.upper.less_equal(chosen_ts)
            }
            TimestampContext::NoTimestamp => true,
        }
    }
}

/// Information used when determining the timestamp for a query.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimestampExplanation<T> {
    /// The chosen timestamp from `determine_timestamp`.
    pub determination: TimestampDetermination<T>,
    /// Details about each source.
    pub sources: Vec<TimestampSource<T>>,
    /// Wall time of first statement executed in this transaction
    pub session_wall_time: DateTime<Utc>,
    /// Cached value of determination.respond_immediately()
    pub respond_immediately: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimestampSource<T> {
    pub name: String,
    pub read_frontier: Vec<T>,
    pub write_frontier: Vec<T>,
}

pub trait DisplayableInTimeline {
    fn fmt(&self, timeline: Option<&Timeline>, f: &mut fmt::Formatter) -> fmt::Result;
    fn display<'a>(&'a self, timeline: Option<&'a Timeline>) -> DisplayInTimeline<'a, Self> {
        DisplayInTimeline { t: self, timeline }
    }
}

impl DisplayableInTimeline for mz_repr::Timestamp {
    fn fmt(&self, timeline: Option<&Timeline>, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(Timeline::EpochMilliseconds) = timeline {
            let ts_ms: u64 = self.into();
            if let Ok(ts_ms) = i64::try_from(ts_ms) {
                if let Some(ndt) = NaiveDateTime::from_timestamp_millis(ts_ms) {
                    return write!(f, "{:13} ({})", self, ndt.format("%Y-%m-%d %H:%M:%S%.3f"));
                }
            }
        }
        write!(f, "{:13}", self)
    }
}

pub struct DisplayInTimeline<'a, T: ?Sized> {
    t: &'a T,
    timeline: Option<&'a Timeline>,
}
impl<'a, T> fmt::Display for DisplayInTimeline<'a, T>
where
    T: DisplayableInTimeline,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.t.fmt(self.timeline, f)
    }
}

impl<'a, T> fmt::Debug for DisplayInTimeline<'a, T>
where
    T: DisplayableInTimeline,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self, f)
    }
}

impl<T: fmt::Display + fmt::Debug + DisplayableInTimeline + TimestampManipulation> fmt::Display
    for TimestampExplanation<T>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let timeline = self.determination.timestamp_context.timeline();
        writeln!(
            f,
            "                query timestamp: {}",
            self.determination
                .timestamp_context
                .timestamp_or_default()
                .display(timeline)
        )?;
        if let Some(oracle_read_ts) = &self.determination.oracle_read_ts {
            writeln!(
                f,
                "          oracle read timestamp: {}",
                oracle_read_ts.display(timeline)
            )?;
        }
        if let Some(session_oracle_read_ts) = &self.determination.session_oracle_read_ts {
            writeln!(
                f,
                "  session oracle read timestamp: {}",
                session_oracle_read_ts.display(timeline)
            )?;
        }
        writeln!(
            f,
            "largest not in advance of upper: {}",
            self.determination
                .largest_not_in_advance_of_upper
                .display(timeline),
        )?;
        writeln!(
            f,
            "                          upper:{:?}",
            self.determination
                .upper
                .iter()
                .map(|t| t.display(timeline))
                .collect::<Vec<_>>()
        )?;
        writeln!(
            f,
            "                          since:{:?}",
            self.determination
                .since
                .iter()
                .map(|t| t.display(timeline))
                .collect::<Vec<_>>()
        )?;
        writeln!(
            f,
            "        can respond immediately: {}",
            self.respond_immediately
        )?;
        writeln!(f, "                       timeline: {:?}", &timeline)?;
        writeln!(
            f,
            "              session wall time: {:13} ({})",
            self.session_wall_time.naive_local().timestamp_millis(),
            self.session_wall_time.format("%Y-%m-%d %H:%M:%S%.3f"),
        )?;

        for source in &self.sources {
            writeln!(f, "")?;
            writeln!(f, "source {}:", source.name)?;
            writeln!(
                f,
                "                  read frontier:{:?}",
                source
                    .read_frontier
                    .iter()
                    .map(|t| t.display(timeline))
                    .collect::<Vec<_>>()
            )?;
            writeln!(
                f,
                "                 write frontier:{:?}",
                source
                    .write_frontier
                    .iter()
                    .map(|t| t.display(timeline))
                    .collect::<Vec<_>>()
            )?;
        }
        Ok(())
    }
}
