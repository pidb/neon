//! WAL receiver logic that ensures the pageserver gets connectected to safekeeper,
//! that contains the latest WAL to stream and this connection does not go stale.
//!
//! To achieve that, a storage broker is used: safekepers propagate their timelines' state in it,
//! the manager subscribes for changes and accumulates those to query the one with the biggest Lsn for connection.
//! Current connection state is tracked too, to ensure it's not getting stale.
//!
//! After every connection or storage broker update fetched, the state gets updated correspondingly and rechecked for the new conneciton leader,
//! then a [re]connection happens, if necessary.
//! Only WAL streaming task expects to be finished, other loops (storage broker, connection management) never exit unless cancelled explicitly via the dedicated channel.

use std::{collections::HashMap, num::NonZeroU64, ops::ControlFlow, sync::Arc, time::Duration};

use crate::task_mgr::TaskKind;
use crate::task_mgr::WALRECEIVER_RUNTIME;
use crate::tenant::Timeline;
use crate::{task_mgr, walreceiver::TaskStateUpdate};
use anyhow::Context;
use chrono::{NaiveDateTime, Utc};
use pageserver_api::models::TimelineState;
use storage_broker::proto::subscribe_safekeeper_info_request::SubscriptionKey;
use storage_broker::proto::SafekeeperTimelineInfo;
use storage_broker::proto::SubscribeSafekeeperInfoRequest;
use storage_broker::proto::TenantTimelineId as ProtoTenantTimelineId;
use storage_broker::BrokerClientChannel;
use storage_broker::Streaming;
use tokio::{select, sync::watch};
use tracing::*;

use crate::{
    exponential_backoff, walreceiver::get_broker_client, DEFAULT_BASE_BACKOFF_SECONDS,
    DEFAULT_MAX_BACKOFF_SECONDS,
};
use postgres_connection::{parse_host_port, PgConnectionConfig};
use utils::{
    id::{NodeId, TenantTimelineId},
    lsn::Lsn,
};

use super::{walreceiver_connection::WalConnectionStatus, TaskEvent, TaskHandle};

/// Spawns the loop to take care of the timeline's WAL streaming connection.
pub fn spawn_connection_manager_task(
    timeline: Arc<Timeline>,
    wal_connect_timeout: Duration,
    lagging_wal_timeout: Duration,
    max_lsn_wal_lag: NonZeroU64,
    auth_token: Option<Arc<String>>,
) {
    let mut broker_client = get_broker_client().clone();

    let tenant_id = timeline.tenant_id;
    let timeline_id = timeline.timeline_id;

    task_mgr::spawn(
        WALRECEIVER_RUNTIME.handle(),
        TaskKind::WalReceiverManager,
        Some(tenant_id),
        Some(timeline_id),
        &format!("walreceiver for timeline {tenant_id}/{timeline_id}"),
        false,
        async move {
            info!("WAL receiver manager started, connecting to broker");
            let mut walreceiver_state = WalreceiverState::new(
                timeline,
                wal_connect_timeout,
                lagging_wal_timeout,
                max_lsn_wal_lag,
                auth_token,
            );
            loop {
                select! {
                    _ = task_mgr::shutdown_watcher() => {
                        info!("WAL receiver shutdown requested, shutting down");
                        walreceiver_state.shutdown().await;
                        return Ok(());
                    },
                    loop_step_result = connection_manager_loop_step(
                        &mut broker_client,
                        &mut walreceiver_state,
                    ) => match loop_step_result {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(()) => {
                            info!("Connection manager loop ended, shutting down");
                            walreceiver_state.shutdown().await;
                            return Ok(());
                        }
                    },
                }
            }
        }
        .instrument(
            info_span!(parent: None, "wal_connection_manager", tenant = %tenant_id, timeline = %timeline_id),
        ),
    );
}

/// Attempts to subscribe for timeline updates, pushed by safekeepers into the broker.
/// Based on the updates, desides whether to start, keep or stop a WAL receiver task.
/// If storage broker subscription is cancelled, exits.
async fn connection_manager_loop_step(
    broker_client: &mut BrokerClientChannel,
    walreceiver_state: &mut WalreceiverState,
) -> ControlFlow<(), ()> {
    let mut timeline_state_updates = walreceiver_state.timeline.subscribe_for_state_updates();

    match wait_for_active_timeline(&mut timeline_state_updates).await {
        ControlFlow::Continue(()) => {}
        ControlFlow::Break(()) => {
            info!("Timeline dropped state updates sender before becoming active, stopping wal connection manager loop");
            return ControlFlow::Break(());
        }
    }

    let id = TenantTimelineId {
        tenant_id: walreceiver_state.timeline.tenant_id,
        timeline_id: walreceiver_state.timeline.timeline_id,
    };

    // Subscribe to the broker updates. Stream shares underlying TCP connection
    // with other streams on this client (other connection managers). When
    // object goes out of scope, stream finishes in drop() automatically.
    let mut broker_subscription = subscribe_for_timeline_updates(broker_client, id).await;
    info!("Subscribed for broker timeline updates");

    loop {
        let time_until_next_retry = walreceiver_state.time_until_next_retry();

        // These things are happening concurrently:
        //
        //  - keep receiving WAL on the current connection
        //      - if the shared state says we need to change connection, disconnect and return
        //      - this runs in a separate task and we receive updates via a watch channel
        //  - change connection if the rules decide so, or if the current connection dies
        //  - receive updates from broker
        //      - this might change the current desired connection
        //  - timeline state changes to something that does not allow walreceiver to run concurrently
        select! {
            Some(wal_connection_update) = async {
                match walreceiver_state.wal_connection.as_mut() {
                    Some(wal_connection) => Some(wal_connection.connection_task.next_task_event().await),
                    None => None,
                }
            } => {
                let wal_connection = walreceiver_state.wal_connection.as_mut()
                    .expect("Should have a connection, as checked by the corresponding select! guard");
                match wal_connection_update {
                    TaskEvent::Update(c) => {
                        match c {
                            TaskStateUpdate::Init | TaskStateUpdate::Started => {},
                            TaskStateUpdate::Progress(status) => {
                                if status.has_processed_wal {
                                    // We have advanced last_record_lsn by processing the WAL received
                                    // from this safekeeper. This is good enough to clean unsuccessful
                                    // retries history and allow reconnecting to this safekeeper without
                                    // sleeping for a long time.
                                    walreceiver_state.wal_connection_retries.remove(&wal_connection.sk_id);
                                }
                                wal_connection.status = status.to_owned();
                            }
                        }
                    },
                    TaskEvent::End(walreceiver_task_result) => {
                        match walreceiver_task_result {
                            Ok(()) => debug!("WAL receiving task finished"),
                            Err(e) => error!("wal receiver task finished with an error: {e:?}"),
                        }
                        walreceiver_state.drop_old_connection(false).await;
                    },
                }
            },

            // Got a new update from the broker
            broker_update = broker_subscription.message() => {
                match broker_update {
                    Ok(Some(broker_update)) => walreceiver_state.register_timeline_update(broker_update),
                    Err(e) => {
                        error!("broker subscription failed: {e}");
                        return ControlFlow::Continue(());
                    }
                    Ok(None) => {
                        error!("broker subscription stream ended"); // can't happen
                        return ControlFlow::Continue(());
                    }
                }
            },

            new_event = async {
                loop {
                    match timeline_state_updates.changed().await {
                        Ok(()) => {
                            let new_state = walreceiver_state.timeline.current_state();
                            match new_state {
                                // we're already active as walreceiver, no need to reactivate
                                TimelineState::Active => continue,
                                TimelineState::Broken | TimelineState::Stopping | TimelineState::Suspended => return ControlFlow::Continue(new_state),
                            }
                        }
                        Err(_sender_dropped_error) => return ControlFlow::Break(()),
                    }
                }
            } => match new_event {
                ControlFlow::Continue(new_state) => {
                    info!("Timeline became inactive (new state: {new_state:?}), dropping current connections until it reactivates");
                    return ControlFlow::Continue(());
                }
                ControlFlow::Break(()) => {
                    info!("Timeline dropped state updates sender, stopping wal connection manager loop");
                    return ControlFlow::Break(());
                }
            },

            _ = async { tokio::time::sleep(time_until_next_retry.unwrap()).await }, if time_until_next_retry.is_some() => {}
        }

        if let Some(new_candidate) = walreceiver_state.next_connection_candidate() {
            info!("Switching to new connection candidate: {new_candidate:?}");
            walreceiver_state
                .change_connection(
                    new_candidate.safekeeper_id,
                    new_candidate.wal_source_connconf,
                )
                .await
        }
    }
}

async fn wait_for_active_timeline(
    timeline_state_updates: &mut watch::Receiver<TimelineState>,
) -> ControlFlow<(), ()> {
    let current_state = *timeline_state_updates.borrow();
    if current_state == TimelineState::Active {
        return ControlFlow::Continue(());
    }

    loop {
        match timeline_state_updates.changed().await {
            Ok(()) => {
                let new_state = *timeline_state_updates.borrow();
                match new_state {
                    TimelineState::Active => {
                        debug!("Timeline state changed to active, continuing the walreceiver connection manager");
                        return ControlFlow::Continue(());
                    }
                    state => {
                        debug!("Not running the walreceiver connection manager, timeline is not active: {state:?}");
                        continue;
                    }
                }
            }
            Err(_sender_dropped_error) => return ControlFlow::Break(()),
        }
    }
}

/// Endlessly try to subscribe for broker updates for a given timeline.
async fn subscribe_for_timeline_updates(
    broker_client: &mut BrokerClientChannel,
    id: TenantTimelineId,
) -> Streaming<SafekeeperTimelineInfo> {
    let mut attempt = 0;
    loop {
        exponential_backoff(
            attempt,
            DEFAULT_BASE_BACKOFF_SECONDS,
            DEFAULT_MAX_BACKOFF_SECONDS,
        )
        .await;
        attempt += 1;

        // subscribe to the specific timeline
        let key = SubscriptionKey::TenantTimelineId(ProtoTenantTimelineId {
            tenant_id: id.tenant_id.as_ref().to_owned(),
            timeline_id: id.timeline_id.as_ref().to_owned(),
        });
        let request = SubscribeSafekeeperInfoRequest {
            subscription_key: Some(key),
        };

        match broker_client.subscribe_safekeeper_info(request).await {
            Ok(resp) => {
                return resp.into_inner();
            }
            Err(e) => {
                warn!("Attempt #{attempt}, failed to subscribe for timeline {id} updates in broker: {e:#}");
                continue;
            }
        }
    }
}

const WALCONNECTION_RETRY_MIN_BACKOFF_SECONDS: f64 = 0.1;
const WALCONNECTION_RETRY_MAX_BACKOFF_SECONDS: f64 = 15.0;
const WALCONNECTION_RETRY_BACKOFF_MULTIPLIER: f64 = 1.5;

/// All data that's needed to run endless broker loop and keep the WAL streaming connection alive, if possible.
struct WalreceiverState {
    id: TenantTimelineId,

    /// Use pageserver data about the timeline to filter out some of the safekeepers.
    timeline: Arc<Timeline>,
    /// The timeout on the connection to safekeeper for WAL streaming.
    wal_connect_timeout: Duration,
    /// The timeout to use to determine when the current connection is "stale" and reconnect to the other one.
    lagging_wal_timeout: Duration,
    /// The Lsn lag to use to determine when the current connection is lagging to much behind and reconnect to the other one.
    max_lsn_wal_lag: NonZeroU64,
    /// Current connection to safekeeper for WAL streaming.
    wal_connection: Option<WalConnection>,
    /// Info about retries and unsuccessful attempts to connect to safekeepers.
    wal_connection_retries: HashMap<NodeId, RetryInfo>,
    /// Data about all timelines, available for connection, fetched from storage broker, grouped by their corresponding safekeeper node id.
    wal_stream_candidates: HashMap<NodeId, BrokerSkTimeline>,
    auth_token: Option<Arc<String>>,
}

/// Current connection data.
#[derive(Debug)]
struct WalConnection {
    /// Time when the connection was initiated.
    started_at: NaiveDateTime,
    /// Current safekeeper pageserver is connected to for WAL streaming.
    sk_id: NodeId,
    /// Status of the connection.
    status: WalConnectionStatus,
    /// WAL streaming task handle.
    connection_task: TaskHandle<WalConnectionStatus>,
    /// Have we discovered that other safekeeper has more recent WAL than we do?
    discovered_new_wal: Option<NewCommittedWAL>,
}

/// Notion of a new committed WAL, which exists on other safekeeper.
#[derive(Debug, Clone, Copy)]
struct NewCommittedWAL {
    /// LSN of the new committed WAL.
    lsn: Lsn,
    /// When we discovered that the new committed WAL exists on other safekeeper.
    discovered_at: NaiveDateTime,
}

#[derive(Debug)]
struct RetryInfo {
    next_retry_at: Option<NaiveDateTime>,
    retry_duration_seconds: f64,
}

/// Data about the timeline to connect to, received from the broker.
#[derive(Debug)]
struct BrokerSkTimeline {
    timeline: SafekeeperTimelineInfo,
    /// Time at which the data was fetched from the broker last time, to track the stale data.
    latest_update: NaiveDateTime,
}

impl WalreceiverState {
    fn new(
        timeline: Arc<Timeline>,
        wal_connect_timeout: Duration,
        lagging_wal_timeout: Duration,
        max_lsn_wal_lag: NonZeroU64,
        auth_token: Option<Arc<String>>,
    ) -> Self {
        let id = TenantTimelineId {
            tenant_id: timeline.tenant_id,
            timeline_id: timeline.timeline_id,
        };
        Self {
            id,
            timeline,
            wal_connect_timeout,
            lagging_wal_timeout,
            max_lsn_wal_lag,
            wal_connection: None,
            wal_stream_candidates: HashMap::new(),
            wal_connection_retries: HashMap::new(),
            auth_token,
        }
    }

    /// Shuts down the current connection (if any) and immediately starts another one with the given connection string.
    async fn change_connection(
        &mut self,
        new_sk_id: NodeId,
        new_wal_source_connconf: PgConnectionConfig,
    ) {
        self.drop_old_connection(true).await;

        let id = self.id;
        let connect_timeout = self.wal_connect_timeout;
        let timeline = Arc::clone(&self.timeline);
        let connection_handle = TaskHandle::spawn(move |events_sender, cancellation| {
            async move {
                super::walreceiver_connection::handle_walreceiver_connection(
                    timeline,
                    new_wal_source_connconf,
                    events_sender,
                    cancellation,
                    connect_timeout,
                )
                .await
                .context("walreceiver connection handling failure")
            }
            .instrument(info_span!("walreceiver_connection", id = %id))
        });

        let now = Utc::now().naive_utc();
        self.wal_connection = Some(WalConnection {
            started_at: now,
            sk_id: new_sk_id,
            status: WalConnectionStatus {
                is_connected: false,
                has_processed_wal: false,
                latest_connection_update: now,
                latest_wal_update: now,
                streaming_lsn: None,
                commit_lsn: None,
            },
            connection_task: connection_handle,
            discovered_new_wal: None,
        });
    }

    /// Drops the current connection (if any) and updates retry timeout for the next
    /// connection attempt to the same safekeeper.
    async fn drop_old_connection(&mut self, needs_shutdown: bool) {
        let wal_connection = match self.wal_connection.take() {
            Some(wal_connection) => wal_connection,
            None => return,
        };

        if needs_shutdown {
            wal_connection.connection_task.shutdown().await;
        }

        let retry = self
            .wal_connection_retries
            .entry(wal_connection.sk_id)
            .or_insert(RetryInfo {
                next_retry_at: None,
                retry_duration_seconds: WALCONNECTION_RETRY_MIN_BACKOFF_SECONDS,
            });

        let now = Utc::now().naive_utc();

        // Schedule the next retry attempt. We want to have exponential backoff for connection attempts,
        // and we add backoff to the time when we started the connection attempt. If the connection
        // was active for a long time, then next_retry_at will be in the past.
        retry.next_retry_at =
            wal_connection
                .started_at
                .checked_add_signed(chrono::Duration::milliseconds(
                    (retry.retry_duration_seconds * 1000.0) as i64,
                ));

        if let Some(next) = &retry.next_retry_at {
            if next > &now {
                info!(
                    "Next connection retry to {:?} is at {}",
                    wal_connection.sk_id, next
                );
            }
        }

        let next_retry_duration =
            retry.retry_duration_seconds * WALCONNECTION_RETRY_BACKOFF_MULTIPLIER;
        // Clamp the next retry duration to the maximum allowed.
        let next_retry_duration = next_retry_duration.min(WALCONNECTION_RETRY_MAX_BACKOFF_SECONDS);
        // Clamp the next retry duration to the minimum allowed.
        let next_retry_duration = next_retry_duration.max(WALCONNECTION_RETRY_MIN_BACKOFF_SECONDS);

        retry.retry_duration_seconds = next_retry_duration;
    }

    /// Returns time needed to wait to have a new candidate for WAL streaming.
    fn time_until_next_retry(&self) -> Option<Duration> {
        let now = Utc::now().naive_utc();

        let next_retry_at = self
            .wal_connection_retries
            .values()
            .filter_map(|retry| retry.next_retry_at)
            .filter(|next_retry_at| next_retry_at > &now)
            .min();

        next_retry_at.and_then(|next_retry_at| (next_retry_at - now).to_std().ok())
    }

    /// Adds another broker timeline into the state, if its more recent than the one already added there for the same key.
    fn register_timeline_update(&mut self, timeline_update: SafekeeperTimelineInfo) {
        self.wal_stream_candidates.insert(
            NodeId(timeline_update.safekeeper_id),
            BrokerSkTimeline {
                timeline: timeline_update,
                latest_update: Utc::now().naive_utc(),
            },
        );
    }

    /// Cleans up stale broker records and checks the rest for the new connection candidate.
    /// Returns a new candidate, if the current state is absent or somewhat lagging, `None` otherwise.
    /// The current rules for approving new candidates:
    /// * pick a candidate different from the connected safekeeper with biggest `commit_lsn` and lowest failed connection attemps
    /// * if there's no such entry, no new candidate found, abort
    /// * otherwise check if the candidate is much better than the current one
    ///
    /// To understand exact rules for determining if the candidate is better than the current one, refer to this function's implementation.
    /// General rules are following:
    /// * if connected safekeeper is not present, pick the candidate
    /// * if we haven't received any updates for some time, pick the candidate
    /// * if the candidate commit_lsn is much higher than the current one, pick the candidate
    /// * if connected safekeeper stopped sending us new WAL which is available on other safekeeper, pick the candidate
    ///
    /// This way we ensure to keep up with the most up-to-date safekeeper and don't try to jump from one safekeeper to another too frequently.
    /// Both thresholds are configured per tenant.
    fn next_connection_candidate(&mut self) -> Option<NewWalConnectionCandidate> {
        self.cleanup_old_candidates();

        match &self.wal_connection {
            Some(existing_wal_connection) => {
                let connected_sk_node = existing_wal_connection.sk_id;

                let (new_sk_id, new_safekeeper_broker_data, new_wal_source_connconf) =
                    self.select_connection_candidate(Some(connected_sk_node))?;

                let now = Utc::now().naive_utc();
                if let Ok(latest_interaciton) =
                    (now - existing_wal_connection.status.latest_connection_update).to_std()
                {
                    // Drop connection if we haven't received keepalive message for a while.
                    if latest_interaciton > self.wal_connect_timeout {
                        return Some(NewWalConnectionCandidate {
                            safekeeper_id: new_sk_id,
                            wal_source_connconf: new_wal_source_connconf,
                            reason: ReconnectReason::NoKeepAlives {
                                last_keep_alive: Some(
                                    existing_wal_connection.status.latest_connection_update,
                                ),
                                check_time: now,
                                threshold: self.wal_connect_timeout,
                            },
                        });
                    }
                }

                if !existing_wal_connection.status.is_connected {
                    // We haven't connected yet and we shouldn't switch until connection timeout (condition above).
                    return None;
                }

                if let Some(current_commit_lsn) = existing_wal_connection.status.commit_lsn {
                    let new_commit_lsn = Lsn(new_safekeeper_broker_data.commit_lsn);
                    // Check if the new candidate has much more WAL than the current one.
                    match new_commit_lsn.0.checked_sub(current_commit_lsn.0) {
                        Some(new_sk_lsn_advantage) => {
                            if new_sk_lsn_advantage >= self.max_lsn_wal_lag.get() {
                                return Some(NewWalConnectionCandidate {
                                    safekeeper_id: new_sk_id,
                                    wal_source_connconf: new_wal_source_connconf,
                                    reason: ReconnectReason::LaggingWal {
                                        current_commit_lsn,
                                        new_commit_lsn,
                                        threshold: self.max_lsn_wal_lag,
                                    },
                                });
                            }
                        }
                        None => debug!(
                            "Best SK candidate has its commit_lsn behind connected SK's commit_lsn"
                        ),
                    }
                }

                let current_lsn = match existing_wal_connection.status.streaming_lsn {
                    Some(lsn) => lsn,
                    None => self.timeline.get_last_record_lsn(),
                };
                let current_commit_lsn = existing_wal_connection
                    .status
                    .commit_lsn
                    .unwrap_or(current_lsn);
                let candidate_commit_lsn = Lsn(new_safekeeper_broker_data.commit_lsn);

                // Keep discovered_new_wal only if connected safekeeper has not caught up yet.
                let mut discovered_new_wal = existing_wal_connection
                    .discovered_new_wal
                    .filter(|new_wal| new_wal.lsn > current_commit_lsn);

                if discovered_new_wal.is_none() {
                    // Check if the new candidate has more WAL than the current one.
                    // If the new candidate has more WAL than the current one, we consider switching to the new candidate.
                    discovered_new_wal = if candidate_commit_lsn > current_commit_lsn {
                        trace!(
                            "New candidate has commit_lsn {}, higher than current_commit_lsn {}",
                            candidate_commit_lsn,
                            current_commit_lsn
                        );
                        Some(NewCommittedWAL {
                            lsn: candidate_commit_lsn,
                            discovered_at: Utc::now().naive_utc(),
                        })
                    } else {
                        None
                    };
                }

                let waiting_for_new_lsn_since = if current_lsn < current_commit_lsn {
                    // Connected safekeeper has more WAL, but we haven't received updates for some time.
                    trace!(
                        "Connected safekeeper has more WAL, but we haven't received updates for {:?}. current_lsn: {}, current_commit_lsn: {}",
                        (now - existing_wal_connection.status.latest_wal_update).to_std(),
                        current_lsn,
                        current_commit_lsn
                    );
                    Some(existing_wal_connection.status.latest_wal_update)
                } else {
                    discovered_new_wal.as_ref().map(|new_wal| {
                        // We know that new WAL is available on other safekeeper, but connected safekeeper don't have it.
                        new_wal
                            .discovered_at
                            .max(existing_wal_connection.status.latest_wal_update)
                    })
                };

                // If we haven't received any WAL updates for a while and candidate has more WAL, switch to it.
                if let Some(waiting_for_new_lsn_since) = waiting_for_new_lsn_since {
                    if let Ok(waiting_for_new_wal) = (now - waiting_for_new_lsn_since).to_std() {
                        if candidate_commit_lsn > current_commit_lsn
                            && waiting_for_new_wal > self.lagging_wal_timeout
                        {
                            return Some(NewWalConnectionCandidate {
                                safekeeper_id: new_sk_id,
                                wal_source_connconf: new_wal_source_connconf,
                                reason: ReconnectReason::NoWalTimeout {
                                    current_lsn,
                                    current_commit_lsn,
                                    candidate_commit_lsn,
                                    last_wal_interaction: Some(
                                        existing_wal_connection.status.latest_wal_update,
                                    ),
                                    check_time: now,
                                    threshold: self.lagging_wal_timeout,
                                },
                            });
                        }
                    }
                }

                self.wal_connection.as_mut().unwrap().discovered_new_wal = discovered_new_wal;
            }
            None => {
                let (new_sk_id, _, new_wal_source_connconf) =
                    self.select_connection_candidate(None)?;
                return Some(NewWalConnectionCandidate {
                    safekeeper_id: new_sk_id,
                    wal_source_connconf: new_wal_source_connconf,
                    reason: ReconnectReason::NoExistingConnection,
                });
            }
        }

        None
    }

    /// Selects the best possible candidate, based on the data collected from the broker updates about the safekeepers.
    /// Optionally, omits the given node, to support gracefully switching from a healthy safekeeper to another.
    ///
    /// The candidate that is chosen:
    /// * has no pending retry cooldown
    /// * has greatest commit_lsn among the ones that are left
    fn select_connection_candidate(
        &self,
        node_to_omit: Option<NodeId>,
    ) -> Option<(NodeId, &SafekeeperTimelineInfo, PgConnectionConfig)> {
        self.applicable_connection_candidates()
            .filter(|&(sk_id, _, _)| Some(sk_id) != node_to_omit)
            .max_by_key(|(_, info, _)| info.commit_lsn)
    }

    /// Returns a list of safekeepers that have valid info and ready for connection.
    /// Some safekeepers are filtered by the retry cooldown.
    fn applicable_connection_candidates(
        &self,
    ) -> impl Iterator<Item = (NodeId, &SafekeeperTimelineInfo, PgConnectionConfig)> {
        let now = Utc::now().naive_utc();

        self.wal_stream_candidates
            .iter()
            .filter(|(_, info)| Lsn(info.timeline.commit_lsn) != Lsn::INVALID)
            .filter(move |(sk_id, _)| {
                let next_retry_at = self
                    .wal_connection_retries
                    .get(sk_id)
                    .and_then(|retry_info| {
                        retry_info.next_retry_at
                    });

                next_retry_at.is_none() || next_retry_at.unwrap() <= now
            }).filter_map(|(sk_id, broker_info)| {
                let info = &broker_info.timeline;
                if info.safekeeper_connstr.is_empty() {
                    return None; // no connection string, ignore sk
                }
                match wal_stream_connection_config(
                    self.id,
                    info.safekeeper_connstr.as_ref(),
                    match &self.auth_token {
                        None => None,
                        Some(x) => Some(x),
                    },
                ) {
                    Ok(connstr) => Some((*sk_id, info, connstr)),
                    Err(e) => {
                        error!("Failed to create wal receiver connection string from broker data of safekeeper node {}: {e:#}", sk_id);
                        None
                    }
                }
            })
    }

    /// Remove candidates which haven't sent broker updates for a while.
    fn cleanup_old_candidates(&mut self) {
        let mut node_ids_to_remove = Vec::with_capacity(self.wal_stream_candidates.len());

        self.wal_stream_candidates.retain(|node_id, broker_info| {
            if let Ok(time_since_latest_broker_update) =
                (Utc::now().naive_utc() - broker_info.latest_update).to_std()
            {
                let should_retain = time_since_latest_broker_update < self.lagging_wal_timeout;
                if !should_retain {
                    node_ids_to_remove.push(*node_id);
                }
                should_retain
            } else {
                true
            }
        });

        for node_id in node_ids_to_remove {
            self.wal_connection_retries.remove(&node_id);
        }
    }

    async fn shutdown(mut self) {
        if let Some(wal_connection) = self.wal_connection.take() {
            wal_connection.connection_task.shutdown().await;
        }
    }
}

#[derive(Debug)]
struct NewWalConnectionCandidate {
    safekeeper_id: NodeId,
    wal_source_connconf: PgConnectionConfig,
    // This field is used in `derive(Debug)` only.
    #[allow(dead_code)]
    reason: ReconnectReason,
}

/// Stores the reason why WAL connection was switched, for furter debugging purposes.
#[derive(Debug, PartialEq, Eq)]
enum ReconnectReason {
    NoExistingConnection,
    LaggingWal {
        current_commit_lsn: Lsn,
        new_commit_lsn: Lsn,
        threshold: NonZeroU64,
    },
    NoWalTimeout {
        current_lsn: Lsn,
        current_commit_lsn: Lsn,
        candidate_commit_lsn: Lsn,
        last_wal_interaction: Option<NaiveDateTime>,
        check_time: NaiveDateTime,
        threshold: Duration,
    },
    NoKeepAlives {
        last_keep_alive: Option<NaiveDateTime>,
        check_time: NaiveDateTime,
        threshold: Duration,
    },
}

fn wal_stream_connection_config(
    TenantTimelineId {
        tenant_id,
        timeline_id,
    }: TenantTimelineId,
    listen_pg_addr_str: &str,
    auth_token: Option<&str>,
) -> anyhow::Result<PgConnectionConfig> {
    let (host, port) =
        parse_host_port(&listen_pg_addr_str).context("Unable to parse listen_pg_addr_str")?;
    let port = port.unwrap_or(5432);
    Ok(PgConnectionConfig::new_host_port(host, port)
        .extend_options([
            "-c".to_owned(),
            format!("timeline_id={}", timeline_id),
            format!("tenant_id={}", tenant_id),
        ])
        .set_password(auth_token.map(|s| s.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::harness::{TenantHarness, TIMELINE_ID};
    use url::Host;

    fn dummy_broker_sk_timeline(
        commit_lsn: u64,
        safekeeper_connstr: &str,
        latest_update: NaiveDateTime,
    ) -> BrokerSkTimeline {
        BrokerSkTimeline {
            timeline: SafekeeperTimelineInfo {
                safekeeper_id: 0,
                tenant_timeline_id: None,
                last_log_term: 0,
                flush_lsn: 0,
                commit_lsn,
                backup_lsn: 0,
                remote_consistent_lsn: 0,
                peer_horizon_lsn: 0,
                local_start_lsn: 0,
                safekeeper_connstr: safekeeper_connstr.to_owned(),
            },
            latest_update,
        }
    }

    #[tokio::test]
    async fn no_connection_no_candidate() -> anyhow::Result<()> {
        let harness = TenantHarness::create("no_connection_no_candidate")?;
        let mut state = dummy_state(&harness).await;
        let now = Utc::now().naive_utc();

        let lagging_wal_timeout = chrono::Duration::from_std(state.lagging_wal_timeout)?;
        let delay_over_threshold = now - lagging_wal_timeout - lagging_wal_timeout;

        state.wal_connection = None;
        state.wal_stream_candidates = HashMap::from([
            (NodeId(0), dummy_broker_sk_timeline(1, "", now)),
            (NodeId(1), dummy_broker_sk_timeline(0, "no_commit_lsn", now)),
            (NodeId(2), dummy_broker_sk_timeline(0, "no_commit_lsn", now)),
            (
                NodeId(3),
                dummy_broker_sk_timeline(
                    1 + state.max_lsn_wal_lag.get(),
                    "delay_over_threshold",
                    delay_over_threshold,
                ),
            ),
        ]);

        let no_candidate = state.next_connection_candidate();
        assert!(
            no_candidate.is_none(),
            "Expected no candidate selected out of non full data options, but got {no_candidate:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn connection_no_candidate() -> anyhow::Result<()> {
        let harness = TenantHarness::create("connection_no_candidate")?;
        let mut state = dummy_state(&harness).await;
        let now = Utc::now().naive_utc();

        let connected_sk_id = NodeId(0);
        let current_lsn = 100_000;

        let connection_status = WalConnectionStatus {
            is_connected: true,
            has_processed_wal: true,
            latest_connection_update: now,
            latest_wal_update: now,
            commit_lsn: Some(Lsn(current_lsn)),
            streaming_lsn: Some(Lsn(current_lsn)),
        };

        state.max_lsn_wal_lag = NonZeroU64::new(100).unwrap();
        state.wal_connection = Some(WalConnection {
            started_at: now,
            sk_id: connected_sk_id,
            status: connection_status.clone(),
            connection_task: TaskHandle::spawn(move |sender, _| async move {
                sender
                    .send(TaskStateUpdate::Progress(connection_status.clone()))
                    .ok();
                Ok(())
            }),
            discovered_new_wal: None,
        });
        state.wal_stream_candidates = HashMap::from([
            (
                connected_sk_id,
                dummy_broker_sk_timeline(
                    current_lsn + state.max_lsn_wal_lag.get() * 2,
                    DUMMY_SAFEKEEPER_HOST,
                    now,
                ),
            ),
            (
                NodeId(1),
                dummy_broker_sk_timeline(current_lsn, "not_advanced_lsn", now),
            ),
            (
                NodeId(2),
                dummy_broker_sk_timeline(
                    current_lsn + state.max_lsn_wal_lag.get() / 2,
                    "not_enough_advanced_lsn",
                    now,
                ),
            ),
        ]);

        let no_candidate = state.next_connection_candidate();
        assert!(
            no_candidate.is_none(),
            "Expected no candidate selected out of valid options since candidate Lsn data is ignored and others' was not advanced enough, but got {no_candidate:?}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn no_connection_candidate() -> anyhow::Result<()> {
        let harness = TenantHarness::create("no_connection_candidate")?;
        let mut state = dummy_state(&harness).await;
        let now = Utc::now().naive_utc();

        state.wal_connection = None;
        state.wal_stream_candidates = HashMap::from([(
            NodeId(0),
            dummy_broker_sk_timeline(1 + state.max_lsn_wal_lag.get(), DUMMY_SAFEKEEPER_HOST, now),
        )]);

        let only_candidate = state
            .next_connection_candidate()
            .expect("Expected one candidate selected out of the only data option, but got none");
        assert_eq!(only_candidate.safekeeper_id, NodeId(0));
        assert_eq!(
            only_candidate.reason,
            ReconnectReason::NoExistingConnection,
            "Should select new safekeeper due to missing connection, even if there's also a lag in the wal over the threshold"
        );
        assert_eq!(
            only_candidate.wal_source_connconf.host(),
            &Host::Domain(DUMMY_SAFEKEEPER_HOST.to_owned())
        );

        let selected_lsn = 100_000;
        state.wal_stream_candidates = HashMap::from([
            (
                NodeId(0),
                dummy_broker_sk_timeline(selected_lsn - 100, "smaller_commit_lsn", now),
            ),
            (
                NodeId(1),
                dummy_broker_sk_timeline(selected_lsn, DUMMY_SAFEKEEPER_HOST, now),
            ),
            (
                NodeId(2),
                dummy_broker_sk_timeline(selected_lsn + 100, "", now),
            ),
        ]);
        let biggest_wal_candidate = state.next_connection_candidate().expect(
            "Expected one candidate selected out of multiple valid data options, but got none",
        );

        assert_eq!(biggest_wal_candidate.safekeeper_id, NodeId(1));
        assert_eq!(
            biggest_wal_candidate.reason,
            ReconnectReason::NoExistingConnection,
            "Should select new safekeeper due to missing connection, even if there's also a lag in the wal over the threshold"
        );
        assert_eq!(
            biggest_wal_candidate.wal_source_connconf.host(),
            &Host::Domain(DUMMY_SAFEKEEPER_HOST.to_owned())
        );

        Ok(())
    }

    #[tokio::test]
    async fn candidate_with_many_connection_failures() -> anyhow::Result<()> {
        let harness = TenantHarness::create("candidate_with_many_connection_failures")?;
        let mut state = dummy_state(&harness).await;
        let now = Utc::now().naive_utc();

        let current_lsn = Lsn(100_000).align();
        let bigger_lsn = Lsn(current_lsn.0 + 100).align();

        state.wal_connection = None;
        state.wal_stream_candidates = HashMap::from([
            (
                NodeId(0),
                dummy_broker_sk_timeline(bigger_lsn.0, DUMMY_SAFEKEEPER_HOST, now),
            ),
            (
                NodeId(1),
                dummy_broker_sk_timeline(current_lsn.0, DUMMY_SAFEKEEPER_HOST, now),
            ),
        ]);
        state.wal_connection_retries = HashMap::from([(
            NodeId(0),
            RetryInfo {
                next_retry_at: now.checked_add_signed(chrono::Duration::hours(1)),
                retry_duration_seconds: WALCONNECTION_RETRY_MAX_BACKOFF_SECONDS,
            },
        )]);

        let candidate_with_less_errors = state
            .next_connection_candidate()
            .expect("Expected one candidate selected, but got none");
        assert_eq!(
            candidate_with_less_errors.safekeeper_id,
            NodeId(1),
            "Should select the node with no pending retry cooldown"
        );

        Ok(())
    }

    #[tokio::test]
    async fn lsn_wal_over_threshhold_current_candidate() -> anyhow::Result<()> {
        let harness = TenantHarness::create("lsn_wal_over_threshcurrent_candidate")?;
        let mut state = dummy_state(&harness).await;
        let current_lsn = Lsn(100_000).align();
        let now = Utc::now().naive_utc();

        let connected_sk_id = NodeId(0);
        let new_lsn = Lsn(current_lsn.0 + state.max_lsn_wal_lag.get() + 1);

        let connection_status = WalConnectionStatus {
            is_connected: true,
            has_processed_wal: true,
            latest_connection_update: now,
            latest_wal_update: now,
            commit_lsn: Some(current_lsn),
            streaming_lsn: Some(current_lsn),
        };

        state.wal_connection = Some(WalConnection {
            started_at: now,
            sk_id: connected_sk_id,
            status: connection_status.clone(),
            connection_task: TaskHandle::spawn(move |sender, _| async move {
                sender
                    .send(TaskStateUpdate::Progress(connection_status.clone()))
                    .ok();
                Ok(())
            }),
            discovered_new_wal: None,
        });
        state.wal_stream_candidates = HashMap::from([
            (
                connected_sk_id,
                dummy_broker_sk_timeline(current_lsn.0, DUMMY_SAFEKEEPER_HOST, now),
            ),
            (
                NodeId(1),
                dummy_broker_sk_timeline(new_lsn.0, "advanced_by_lsn_safekeeper", now),
            ),
        ]);

        let over_threshcurrent_candidate = state.next_connection_candidate().expect(
            "Expected one candidate selected out of multiple valid data options, but got none",
        );

        assert_eq!(over_threshcurrent_candidate.safekeeper_id, NodeId(1));
        assert_eq!(
            over_threshcurrent_candidate.reason,
            ReconnectReason::LaggingWal {
                current_commit_lsn: current_lsn,
                new_commit_lsn: new_lsn,
                threshold: state.max_lsn_wal_lag
            },
            "Should select bigger WAL safekeeper if it starts to lag enough"
        );
        assert_eq!(
            over_threshcurrent_candidate.wal_source_connconf.host(),
            &Host::Domain("advanced_by_lsn_safekeeper".to_owned())
        );

        Ok(())
    }

    #[tokio::test]
    async fn timeout_connection_threshhold_current_candidate() -> anyhow::Result<()> {
        let harness = TenantHarness::create("timeout_connection_threshhold_current_candidate")?;
        let mut state = dummy_state(&harness).await;
        let current_lsn = Lsn(100_000).align();
        let now = Utc::now().naive_utc();

        let wal_connect_timeout = chrono::Duration::from_std(state.wal_connect_timeout)?;
        let time_over_threshold =
            Utc::now().naive_utc() - wal_connect_timeout - wal_connect_timeout;

        let connection_status = WalConnectionStatus {
            is_connected: true,
            has_processed_wal: true,
            latest_connection_update: time_over_threshold,
            latest_wal_update: time_over_threshold,
            commit_lsn: Some(current_lsn),
            streaming_lsn: Some(current_lsn),
        };

        state.wal_connection = Some(WalConnection {
            started_at: now,
            sk_id: NodeId(1),
            status: connection_status.clone(),
            connection_task: TaskHandle::spawn(move |sender, _| async move {
                sender
                    .send(TaskStateUpdate::Progress(connection_status.clone()))
                    .ok();
                Ok(())
            }),
            discovered_new_wal: None,
        });
        state.wal_stream_candidates = HashMap::from([(
            NodeId(0),
            dummy_broker_sk_timeline(current_lsn.0, DUMMY_SAFEKEEPER_HOST, now),
        )]);

        let over_threshcurrent_candidate = state.next_connection_candidate().expect(
            "Expected one candidate selected out of multiple valid data options, but got none",
        );

        assert_eq!(over_threshcurrent_candidate.safekeeper_id, NodeId(0));
        match over_threshcurrent_candidate.reason {
            ReconnectReason::NoKeepAlives {
                last_keep_alive,
                threshold,
                ..
            } => {
                assert_eq!(last_keep_alive, Some(time_over_threshold));
                assert_eq!(threshold, state.lagging_wal_timeout);
            }
            unexpected => panic!("Unexpected reason: {unexpected:?}"),
        }
        assert_eq!(
            over_threshcurrent_candidate.wal_source_connconf.host(),
            &Host::Domain(DUMMY_SAFEKEEPER_HOST.to_owned())
        );

        Ok(())
    }

    #[tokio::test]
    async fn timeout_wal_over_threshhold_current_candidate() -> anyhow::Result<()> {
        let harness = TenantHarness::create("timeout_wal_over_threshhold_current_candidate")?;
        let mut state = dummy_state(&harness).await;
        let current_lsn = Lsn(100_000).align();
        let new_lsn = Lsn(100_100).align();
        let now = Utc::now().naive_utc();

        let lagging_wal_timeout = chrono::Duration::from_std(state.lagging_wal_timeout)?;
        let time_over_threshold =
            Utc::now().naive_utc() - lagging_wal_timeout - lagging_wal_timeout;

        let connection_status = WalConnectionStatus {
            is_connected: true,
            has_processed_wal: true,
            latest_connection_update: now,
            latest_wal_update: time_over_threshold,
            commit_lsn: Some(current_lsn),
            streaming_lsn: Some(current_lsn),
        };

        state.wal_connection = Some(WalConnection {
            started_at: now,
            sk_id: NodeId(1),
            status: connection_status,
            connection_task: TaskHandle::spawn(move |_, _| async move { Ok(()) }),
            discovered_new_wal: Some(NewCommittedWAL {
                discovered_at: time_over_threshold,
                lsn: new_lsn,
            }),
        });
        state.wal_stream_candidates = HashMap::from([(
            NodeId(0),
            dummy_broker_sk_timeline(new_lsn.0, DUMMY_SAFEKEEPER_HOST, now),
        )]);

        let over_threshcurrent_candidate = state.next_connection_candidate().expect(
            "Expected one candidate selected out of multiple valid data options, but got none",
        );

        assert_eq!(over_threshcurrent_candidate.safekeeper_id, NodeId(0));
        match over_threshcurrent_candidate.reason {
            ReconnectReason::NoWalTimeout {
                current_lsn,
                current_commit_lsn,
                candidate_commit_lsn,
                last_wal_interaction,
                threshold,
                ..
            } => {
                assert_eq!(current_lsn, current_lsn);
                assert_eq!(current_commit_lsn, current_lsn);
                assert_eq!(candidate_commit_lsn, new_lsn);
                assert_eq!(last_wal_interaction, Some(time_over_threshold));
                assert_eq!(threshold, state.lagging_wal_timeout);
            }
            unexpected => panic!("Unexpected reason: {unexpected:?}"),
        }
        assert_eq!(
            over_threshcurrent_candidate.wal_source_connconf.host(),
            &Host::Domain(DUMMY_SAFEKEEPER_HOST.to_owned())
        );

        Ok(())
    }

    const DUMMY_SAFEKEEPER_HOST: &str = "safekeeper_connstr";

    async fn dummy_state(harness: &TenantHarness<'_>) -> WalreceiverState {
        WalreceiverState {
            id: TenantTimelineId {
                tenant_id: harness.tenant_id,
                timeline_id: TIMELINE_ID,
            },
            timeline: harness
                .load()
                .await
                .create_empty_timeline(TIMELINE_ID, Lsn(0), crate::DEFAULT_PG_VERSION)
                .expect("Failed to create an empty timeline for dummy wal connection manager")
                .initialize()
                .unwrap(),
            wal_connect_timeout: Duration::from_secs(1),
            lagging_wal_timeout: Duration::from_secs(1),
            max_lsn_wal_lag: NonZeroU64::new(1024 * 1024).unwrap(),
            wal_connection: None,
            wal_stream_candidates: HashMap::new(),
            wal_connection_retries: HashMap::new(),
            auth_token: None,
        }
    }
}
