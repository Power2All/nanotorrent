use std::collections::HashSet;
use std::net::SocketAddr;
use std::net::SocketAddrV4;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use backon::ExponentialBuilder;
use backon::Retryable;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::Either;
use futures::stream::BoxStream;
use futures::stream::FuturesUnordered;
use tracing::Instrument;
use tracing::debug;
use tracing::debug_span;
use tracing::trace;
use tracing::trace_span;
use url::Url;

use crate::tracker_comms_http;
use crate::tracker_comms_udp;
use crate::tracker_comms_udp::UdpTrackerClient;
use librqbit_core::hash_id::Id20;

pub struct TrackerComms {
    info_hash: Id20,
    peer_id: Id20,
    stats: Box<dyn TorrentStatsProvider>,
    force_tracker_interval: Option<Duration>,
    tx: Sender,
    // This MUST be set as trackers don't work with 0 port.
    announce_port: u16,
    reqwest_client: reqwest::Client,
    key: u32,
    // NanoTorrent addition: per-tracker announce stats, keyed by tracker URL.
    tracker_stats: SharedTrackerStats,
}

/// NanoTorrent addition: last-known announce state for one tracker. The
/// upstream crate receives these numbers but discards them; we record them so
/// the UI can show a PicoTorrent-style Trackers tab.
#[derive(Clone, Debug, Default)]
pub struct TrackerStat {
    /// "Working" or an error message. Empty until the first announce settles -
    /// the UI turns that into its own translated "Updating...", so don't put an
    /// English placeholder here.
    pub status: String,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    pub fails: u32,
    pub next_announce: Option<std::time::SystemTime>,
}

/// Shared, mutable map of tracker URL -> its latest stats.
pub type SharedTrackerStats =
    Arc<std::sync::Mutex<std::collections::HashMap<String, TrackerStat>>>;

#[derive(Default)]
pub enum TrackerCommsStatsState {
    #[default]
    None,
    Initializing,
    Paused,
    Live,
}

#[derive(Default)]
pub struct TrackerCommsStats {
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub torrent_state: TrackerCommsStatsState,
}

impl TrackerCommsStats {
    pub fn get_left_to_download_bytes(&self) -> u64 {
        let total = self.total_bytes;
        let down = self.downloaded_bytes;
        if total >= down {
            return total - down;
        }
        0
    }

    pub fn is_completed(&self) -> bool {
        self.downloaded_bytes >= self.total_bytes
    }
}

pub trait TorrentStatsProvider: Send + Sync {
    fn get(&self) -> TrackerCommsStats;
}

impl TorrentStatsProvider for () {
    fn get(&self) -> TrackerCommsStats {
        Default::default()
    }
}

type Sender = tokio::sync::mpsc::Sender<SocketAddr>;

#[derive(Clone)]
enum SupportedTracker {
    Udp(Url),
    Http(Url),
}

impl std::fmt::Debug for SupportedTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupportedTracker::Udp(u) => std::fmt::Display::fmt(u, f),
            SupportedTracker::Http(u) => std::fmt::Display::fmt(u, f),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum UdpTrackerResolveResult {
    One(SocketAddr),
    Two(SocketAddrV4, SocketAddrV6),
}

/// Fisher-Yates, so a tier's URLs are tried in a different order per torrent.
///
/// BEP 12: "the list will be shuffled when first read, and then parsed in
/// order". Without it every client would hammer whichever tracker the author
/// happened to write first.
///
/// Hand-rolled on `rand::random` rather than pulling in `SliceRandom`, which is
/// the only thing this crate would want from `rand::seq`.
fn shuffle<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = (rand::random::<u64>() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

/// NanoTorrent addition: line the recorded announce tiers up with the tracker
/// set that is actually live.
///
/// The map records the grouping the .torrent declared; `live` is what the
/// torrent announces to now, which differs once trackers have been added or
/// removed by hand. Trusting either alone is wrong in a way that is invisible
/// until a torrent goes quiet: the map alone keeps announcing to a tracker the
/// user deleted, and the live set alone throws the grouping away and with it
/// BEP 12's fallback order.
///
/// Every live URL comes out exactly once. Anything the map does not account
/// for goes in a tier of its own at the end, which is where a hand-added
/// tracker belongs anyway - it is not part of any tier the file declared.
pub fn reconcile_tiers(
    mut recorded: Vec<Vec<Url>>,
    live: HashSet<Url>,
) -> Vec<Vec<Url>> {
    for tier in recorded.iter_mut() {
        tier.retain(|u| live.contains(u));
    }
    recorded.retain(|tier| !tier.is_empty());

    let grouped: HashSet<Url> =
        recorded.iter().flatten().cloned().collect();
    let mut ungrouped: Vec<Url> =
        live.into_iter().filter(|u| !grouped.contains(u)).collect();
    if !ungrouped.is_empty() {
        // Sorted only so the result does not depend on HashSet iteration order,
        // which would make this untestable and the logs jump about.
        ungrouped.sort();
        recorded.push(ungrouped);
    }
    recorded
}

#[cfg(test)]
mod nanotorrent_tier_tests {
    use super::{HashSet, Url, reconcile_tiers};

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn urls(ss: &[&str]) -> Vec<Url> {
        ss.iter().map(|s| url(s)).collect()
    }

    fn shape(tiers: &[Vec<Url>]) -> Vec<Vec<String>> {
        tiers
            .iter()
            .map(|t| t.iter().map(|u| u.as_str().to_owned()).collect())
            .collect()
    }

    #[test]
    fn the_recorded_grouping_is_kept_when_nothing_has_changed() {
        let recorded = vec![urls(&["http://a/", "http://b/"]), urls(&["http://c/"])];
        let live = urls(&["http://a/", "http://b/", "http://c/"])
            .into_iter()
            .collect();
        assert_eq!(
            shape(&reconcile_tiers(recorded, live)),
            vec![
                vec!["http://a/".to_owned(), "http://b/".to_owned()],
                vec!["http://c/".to_owned()]
            ]
        );
    }

    #[test]
    fn a_tracker_removed_by_hand_is_not_announced_to() {
        let recorded = vec![urls(&["http://a/", "http://b/"])];
        let live = urls(&["http://a/"]).into_iter().collect();
        assert_eq!(
            shape(&reconcile_tiers(recorded, live)),
            vec![vec!["http://a/".to_owned()]]
        );
    }

    /// A tier that loses every member disappears rather than being announced as
    /// an empty group, which the driver would treat as a tier that can never
    /// succeed and so would sit on forever.
    #[test]
    fn a_tier_emptied_by_removals_disappears() {
        let recorded = vec![urls(&["http://gone/"]), urls(&["http://b/"])];
        let live = urls(&["http://b/"]).into_iter().collect();
        assert_eq!(
            shape(&reconcile_tiers(recorded, live)),
            vec![vec!["http://b/".to_owned()]]
        );
    }

    /// The case that would otherwise lose a tracker silently: one added by
    /// hand, which the recorded map knows nothing about.
    #[test]
    fn a_tracker_the_map_never_saw_still_gets_announced_to() {
        let recorded = vec![urls(&["http://a/"])];
        let live = urls(&["http://a/", "http://added/"]).into_iter().collect();
        assert_eq!(
            shape(&reconcile_tiers(recorded, live)),
            vec![
                vec!["http://a/".to_owned()],
                vec!["http://added/".to_owned()]
            ]
        );
    }

    /// No recording at all - a magnet, or a torrent added before tiers were
    /// captured. Everything lands in one tier, which the driver deliberately
    /// treats as "announce to all".
    #[test]
    fn no_recorded_tiers_gives_one_tier_of_everything() {
        let live = urls(&["http://a/", "http://b/"]).into_iter().collect();
        let out = reconcile_tiers(Vec::new(), live);
        assert_eq!(out.len(), 1, "should be a single tier: {out:?}");
        assert_eq!(out[0].len(), 2);
    }

    /// Whatever goes in comes out - once each. This is the property that makes
    /// the rest safe.
    #[test]
    fn every_live_tracker_appears_exactly_once() {
        let recorded = vec![
            urls(&["http://a/", "http://dead/"]),
            urls(&["http://b/"]),
            urls(&["http://alsodead/"]),
        ];
        let live: HashSet<Url> =
            urls(&["http://a/", "http://b/", "http://new/"])
                .into_iter()
                .collect();
        let out = reconcile_tiers(recorded, live.clone());
        let mut flat: Vec<String> = out
            .iter()
            .flatten()
            .map(|u| u.as_str().to_owned())
            .collect();
        flat.sort();
        let mut want: Vec<String> = live.iter().map(|u| u.as_str().to_owned()).collect();
        want.sort();
        assert_eq!(flat, want);
    }
}

#[cfg(test)]
mod nanotorrent_shuffle_tests {
    use super::shuffle;

    /// Whatever goes in comes out - a tier must not lose or duplicate a tracker
    /// on its way through the shuffle.
    #[test]
    fn shuffling_keeps_every_element() {
        for n in 0..12usize {
            let mut v: Vec<usize> = (0..n).collect();
            shuffle(&mut v);
            v.sort();
            assert_eq!(v, (0..n).collect::<Vec<_>>(), "n = {n}");
        }
    }

    /// And it actually permutes. A shuffle that is a no-op would satisfy the
    /// test above perfectly while leaving every client hammering whichever
    /// tracker the torrent's author happened to write first.
    #[test]
    fn shuffling_actually_reorders() {
        let original: Vec<usize> = (0..16).collect();
        let moved = (0..40).any(|_| {
            let mut v = original.clone();
            shuffle(&mut v);
            v != original
        });
        assert!(moved, "40 shuffles of 16 elements never changed the order");
    }
}

async fn udp_tracker_to_socket_addrs(
    host: url::Host<&str>,
    port: u16,
) -> anyhow::Result<UdpTrackerResolveResult> {
    let res = match host {
        url::Host::Domain(name) => {
            // Use the first IPv4 and the first IPv6 addresses only.

            let mut v4: Option<SocketAddrV4> = None;
            let mut v6: Option<SocketAddrV6> = None;
            for addr in tokio::net::lookup_host((name, port))
                .await
                .with_context(|| format!("error looking up hostname {name}"))?
            {
                match (v4, v6, addr) {
                    (None, _, SocketAddr::V4(addr)) => v4 = Some(addr),
                    (_, None, SocketAddr::V6(addr)) => v6 = Some(addr),
                    _ => continue,
                }
            }
            let res = match (v4, v6) {
                (Some(v4), Some(v6)) => UdpTrackerResolveResult::Two(v4, v6),
                (Some(v4), None) => UdpTrackerResolveResult::One(v4.into()),
                (None, Some(v6)) => UdpTrackerResolveResult::One(v6.into()),
                _ => anyhow::bail!("zero addresses returned looking up {name}"),
            };
            trace!(?res, "resolved");
            res
        }
        url::Host::Ipv4(addr) => UdpTrackerResolveResult::One((addr, port).into()),
        url::Host::Ipv6(addr) => UdpTrackerResolveResult::One((addr, port).into()),
    };
    Ok(res)
}

impl TrackerComms {
    // TODO: fix too many args
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        info_hash: Id20,
        peer_id: Id20,
        // NanoTorrent change: the announce list arrives grouped into TIERS
        // rather than flattened. See `run_tier` for what that buys.
        tiers: Vec<Vec<Url>>,
        stats: Box<dyn TorrentStatsProvider>,
        force_interval: Option<Duration>,
        announce_port: u16,
        reqwest_client: reqwest::Client,
        udp_client: UdpTrackerClient,
        tracker_stats: SharedTrackerStats,
    ) -> Option<BoxStream<'static, SocketAddr>> {
        let tiers: Vec<Vec<SupportedTracker>> = tiers
            .into_iter()
            .map(|tier| {
                tier.into_iter()
                    .filter_map(|t| match t.scheme() {
                        "http" | "https" => Some(SupportedTracker::Http(t)),
                        "udp" => Some(SupportedTracker::Udp(t)),
                        _ => {
                            debug!("unsupported tracker URL: {}", t);
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|tier: &Vec<SupportedTracker>| !tier.is_empty())
            .collect();
        if tiers.is_empty() {
            debug!(?info_hash, "trackers list is empty");
            return None;
        }

        tracing::trace!(?tiers);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<SocketAddr>(16);

        let s = async_stream::stream! {
            use futures::StreamExt;
            let comms = Arc::new(Self {
                info_hash,
                peer_id,
                stats,
                force_tracker_interval: force_interval,
                tx,
                announce_port,
                reqwest_client,
                key: rand::random(),
                tracker_stats,
            });
            let mut futures = FuturesUnordered::new();
            // One tier is the common case and is deliberately NOT treated as a
            // tier: a great many torrents are built with every tracker on one
            // line, which bencodes as a single tier that was never meant as a
            // fallback group. Announcing to only one of those would cut a
            // torrent off from most of its swarm to honour a grouping the
            // author did not intend. With more than one tier the grouping IS
            // deliberate, and BEP 12's order is what the author asked for.
            match tiers.len() {
                1 => {
                    for tracker in tiers.into_iter().next().unwrap_or_default() {
                        futures.push(comms.add_tracker(tracker, &udp_client, None).boxed())
                    }
                }
                _ => {
                    for tier in tiers {
                        futures.push(comms.run_tier(tier, &udp_client).boxed())
                    }
                }
            }
            while !(futures.is_empty()) {
                tokio::select! {
                    addr = rx.recv() => {
                        if let Some(addr) = addr {
                            yield addr;
                        }
                    }
                    e = futures.next(), if !futures.is_empty() => {
                        if let Some(Err(e)) = e {
                            debug!("error: {e}");
                        }
                    }
                }
            }
        };

        Some(s.boxed())
    }

    /// NanoTorrent addition: mutate the stored stats for one tracker URL.
    fn set_stat(&self, key: &str, f: impl FnOnce(&mut TrackerStat)) {
        if let Ok(mut map) = self.tracker_stats.lock() {
            f(map.entry(key.to_string()).or_default());
        }
    }

    /// Work one tier, per BEP 12: shuffled once, then tried in order, and the
    /// first tracker that answers is the only one this tier announces to.
    ///
    /// "First that works" falls out of the monitor's own shape rather than
    /// needing a success signal: a monitor that is announcing happily never
    /// returns, so control stays on that tracker for as long as it keeps
    /// working. `max_failures` is what makes the rest possible - without a
    /// bound the monitor retries the same dead tracker forever (up to ten
    /// minutes between tries) and the tier can never move on, which is why
    /// tiers did nothing here before.
    ///
    /// When a tracker does give up, the next one in the tier takes over; after
    /// the last, it wraps to the first, which by then has had a long time to
    /// come back.
    async fn run_tier(
        &self,
        tier: Vec<SupportedTracker>,
        client: &UdpTrackerClient,
    ) -> anyhow::Result<()> {
        /// How many times a tracker is retried before its tier moves on.
        ///
        /// The retry is exponential from 10s to 600s, so four attempts is
        /// roughly two and a half minutes of trying before the fallback is
        /// used - long enough not to flap on a blip, short enough that a dead
        /// primary does not strand the torrent.
        const MAX_FAILURES: usize = 4;

        let mut order = tier;
        shuffle(&mut order);
        let mut at = 0usize;
        loop {
            let tracker = order[at].clone();
            let err = self
                .add_tracker(tracker, client, Some(MAX_FAILURES))
                .await
                .err();
            debug!(
                tier_position = at,
                "tracker gave up, falling through to the next in its tier: {err:#?}"
            );
            at = (at + 1) % order.len();
        }
    }

    /// `max_failures`: None retries forever, which is right when nothing is
    /// waiting to take over. Some(n) lets the tracker fail so its tier can move
    /// on to the next one.
    fn add_tracker(
        &self,
        url: SupportedTracker,
        client: &UdpTrackerClient,
        max_failures: Option<usize>,
    ) -> Either<
        impl std::future::Future<Output = anyhow::Result<()>> + '_ + Send,
        impl std::future::Future<Output = anyhow::Result<()>> + '_ + Send,
    > {
        let info_hash = self.info_hash;
        match url {
            SupportedTracker::Udp(url) => {
                let span = debug_span!(parent: None, "udp_tracker", tracker = %url, info_hash = ?info_hash);
                self.task_single_tracker_monitor_udp(url, client.clone(), max_failures)
                    .instrument(span)
                    .right_future()
            }
            SupportedTracker::Http(url) => {
                let span = debug_span!(
                    parent: None,
                    "http_tracker",
                    tracker = %url,
                    info_hash = ?info_hash
                );
                self.task_single_tracker_monitor_http(url, max_failures)
                    .instrument(span)
                    .left_future()
            }
        }
    }

    async fn task_single_tracker_monitor_http(
        &self,
        tracker_url: Url,
        max_failures: Option<usize>,
    ) -> anyhow::Result<()> {
        trace!(url=%tracker_url, "starting monitor");
        let mut event = Some(tracker_comms_http::TrackerRequestEvent::Started);

        loop {
            let backoff = ExponentialBuilder::new()
                .with_jitter()
                .with_factor(2.)
                .with_min_delay(Duration::from_secs(10))
                .with_max_delay(Duration::from_secs(600));
            let backoff = match max_failures {
                Some(n) => backoff.with_max_times(n),
                None => backoff.without_max_times(),
            };
            let interval = (|| self.tracker_one_request_http(&tracker_url, event))
                .retry(backoff)
                .notify(|err, retry_in| {
                    debug!(?retry_in, "error calling tracker: {err:#}");
                    // NanoTorrent addition: surface the failure in the UI.
                    self.set_stat(tracker_url.as_str(), |s| {
                        s.status = format!("{err:#}");
                        s.fails += 1;
                        s.next_announce = std::time::SystemTime::now().checked_add(retry_in);
                    });
                })
                .await
                // Reachable only when `max_failures` is set: without a bound
                // the retry never stops, which is what the old message meant.
                .context("giving up on this tracker")?;

            event = None;
            let interval = self.force_tracker_interval.unwrap_or(interval);
            debug!("sleeping for {:?} after calling tracker", interval);
            tokio::time::sleep(interval).await;
        }
    }

    async fn tracker_one_request_http(
        &self,
        tracker_url: &Url,
        event: Option<tracker_comms_http::TrackerRequestEvent>,
    ) -> anyhow::Result<Duration> {
        let stats = self.stats.get();
        let request = tracker_comms_http::TrackerRequest {
            info_hash: &self.info_hash,
            peer_id: &self.peer_id,
            port: self.announce_port,
            uploaded: stats.uploaded_bytes,
            downloaded: stats.downloaded_bytes,
            left: stats.get_left_to_download_bytes(),
            compact: true,
            no_peer_id: false,
            event,
            ip: None,
            numwant: None,
            key: Some(self.key),
            trackerid: None,
        };

        let mut url = tracker_url.clone();

        let mut queries = request.as_querystring();
        if let Some(url_query) = url.query() {
            queries.push_str(&format!("&{}", url_query));
        }
        url.set_query(Some(&queries));

        let response: reqwest::Response = self.reqwest_client.get(url).send().await?;
        if !response.status().is_success() {
            anyhow::bail!("tracker responded with {:?}", response.status());
        }
        let bytes = response.bytes().await?;
        if let Ok((error, _)) =
            bencode::from_bytes_with_rest::<tracker_comms_http::TrackerError>(&bytes)
        {
            anyhow::bail!(
                "tracker returned failure. Failure reason: {}",
                error.failure_reason
            )
        };
        let response = bencode::from_bytes_with_rest::<tracker_comms_http::TrackerResponse>(&bytes)
            .map_err(|e| {
                tracing::trace!("error deserializing TrackerResponse: {e:#}");
                e.into_kind()
            })?
            .0;

        for peer in response.iter_peers() {
            self.tx.send(peer).await?;
        }
        let interval = Duration::from_secs(response.min_interval.unwrap_or(response.interval));
        // NanoTorrent addition: record what the tracker just told us.
        self.set_stat(tracker_url.as_str(), |s| {
            s.status = "Working".to_string();
            s.seeders = Some(response.complete as u32);
            s.leechers = Some(response.incomplete as u32);
            s.next_announce = std::time::SystemTime::now().checked_add(interval);
        });
        Ok(interval)
    }

    async fn task_single_tracker_monitor_udp(
        &self,
        url: Url,
        client: UdpTrackerClient,
        max_failures: Option<usize>,
    ) -> anyhow::Result<()> {
        if url.scheme() != "udp" {
            bail!("expected UDP scheme in {}", url);
        }
        let (host, port) = (
            url.host().context("missing host")?,
            url.port().context("missing port")?,
        );

        let mut sleep_interval: Option<Duration> = None;
        let mut prev_addrs: Option<UdpTrackerResolveResult> = None;
        // Consecutive, not total: a tracker that answers once has proved it is
        // alive, and the count starts again.
        let mut failures = 0usize;
        loop {
            if let Some(i) = sleep_interval {
                trace!(interval=?sleep_interval, "sleeping");
                tokio::time::sleep(i).await;
            }

            // This should retry forever until the addrs are resolved.
            let addrs = (async || {
                udp_tracker_to_socket_addrs(host.clone(), port)
                    .instrument(trace_span!("resolve", ?host))
                    .await
                    .or_else(|err| prev_addrs.ok_or(err))
            })
            .retry(
                ExponentialBuilder::new()
                    .without_max_times()
                    .with_max_delay(Duration::from_secs(60))
                    .with_jitter(),
            )
            .notify(|err, retry| debug!(retry_in=?retry, "error resolving tracker: {err:#}"))
            .await
            .context("this shouldn't happen: failed resolving tracker addrs")?;

            prev_addrs = Some(addrs);

            match addrs {
                UdpTrackerResolveResult::One(addr) => {
                    match self
                        .tracker_one_request_udp(&url, addr, &client)
                        .instrument(trace_span!("udp request", ?addr))
                        .await
                    {
                        Ok(sleep) => {
                            failures = 0;
                            sleep_interval = Some(sleep)
                        }
                        Err(_) => {
                            failures += 1;
                            if max_failures.is_some_and(|max| failures >= max) {
                                bail!("giving up on {url} after {failures} failed announces");
                            }
                            sleep_interval = Some(sleep_interval.unwrap_or(Duration::from_secs(60)))
                        }
                    }
                }
                UdpTrackerResolveResult::Two(v4, v6) => {
                    let (r4, r6) = tokio::join!(
                        self.tracker_one_request_udp(&url, v4.into(), &client)
                            .instrument(trace_span!("udp request", addr=?v4)),
                        self.tracker_one_request_udp(&url, v6.into(), &client)
                            .instrument(trace_span!("udp request", addr=?v6))
                    );
                    // Either address answering counts as the tracker being up:
                    // one URL resolving to both v4 and v6 is one tracker.
                    match r4.is_ok() || r6.is_ok() {
                        true => failures = 0,
                        false => {
                            failures += 1;
                            if max_failures.is_some_and(|max| failures >= max) {
                                bail!("giving up on {url} after {failures} failed announces");
                            }
                        }
                    }
                    sleep_interval = Some(
                        r4.or(r6)
                            .ok()
                            .or(sleep_interval)
                            .unwrap_or(Duration::from_secs(60)),
                    )
                }
            }
        }
    }

    async fn tracker_one_request_udp(
        &self,
        // NanoTorrent addition: the announce URL, used only as the stats key.
        // The UI shows one row per tracker URL, but a single URL can resolve to
        // both a v4 and a v6 address and is then announced to twice.
        url: &Url,
        addr: SocketAddr,
        client: &UdpTrackerClient,
    ) -> anyhow::Result<Duration> {
        use tracker_comms_udp::*;

        let stats = self.stats.get();
        let request = AnnounceFields {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            downloaded: stats.downloaded_bytes,
            left: stats.get_left_to_download_bytes(),
            uploaded: stats.uploaded_bytes,
            event: match stats.torrent_state {
                TrackerCommsStatsState::None => EVENT_NONE,
                TrackerCommsStatsState::Initializing => EVENT_STARTED,
                TrackerCommsStatsState::Paused => EVENT_STOPPED,
                TrackerCommsStatsState::Live => {
                    if stats.is_completed() {
                        EVENT_COMPLETED
                    } else {
                        EVENT_STARTED
                    }
                }
            },
            key: self.key,
            port: self.announce_port,
        };

        match client.announce(addr, request).await {
            Ok(response) => {
                trace!(len = response.addrs.len(), "received announce response");
                let (seeders, leechers) = (response.seeders, response.leechers);
                for addr in response.addrs {
                    self.tx.send(addr).await.context("rx closed")?;
                }
                let sleep = response.interval.max(5);
                let sleep = Duration::from_secs(sleep as u64);
                // NanoTorrent addition: record what the tracker just told us.
                self.set_stat(url.as_str(), |s| {
                    s.status = "Working".to_string();
                    s.seeders = Some(seeders);
                    s.leechers = Some(leechers);
                    s.next_announce = std::time::SystemTime::now().checked_add(sleep);
                });
                Ok(sleep)
            }
            Err(e) => {
                debug!(?addr, "error reading announce response: {e:#}");
                // NanoTorrent addition: surface the failure in the Trackers tab.
                self.set_stat(url.as_str(), |s| {
                    s.status = format!("{e:#}");
                    s.fails += 1;
                });
                Err(e)
            }
        }
    }
}
