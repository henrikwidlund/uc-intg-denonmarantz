// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! HEOS player of a receiver: what plays on network audio, and play, pause, stop, next and
//! previous for it. Volume and mute are not in HEOS: the zones share its volume. The library opens
//! the connection and finds the player. The driver tries the first connect again.

use crate::device::discovery::{NETWORK_RETRY_INTERVAL, NETWORK_RETRY_TIMEOUT};
use crate::errors::ServiceError;
use denon_marantz::heos::{Change, Heos, PlayMode, PlayState, PlayerId, PlayerState, RepeatMode};
use denon_marantz::{Avr, HeosFailure, HeosPlayer, HeosPlayerStatus};
use log::{debug, warn};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::time::sleep;

/// Waits before HEOS is asked again after a failed first connect. The last wait repeats. After a
/// connect, the HEOS client connects again by itself.
#[cfg(not(test))]
const HEOS_WAITS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
#[cfg(test)]
const HEOS_WAITS: [Duration; 5] = [Duration::from_millis(100); 5];

/// The HEOS player of a receiver. It does not keep the connection open: the `Avr` owns it.
#[derive(Debug, Clone)]
pub struct Link {
    heos: Weak<Heos>,
    pid: PlayerId,
}

impl Link {
    /// `None` if the connection is closed.
    pub fn new(player: &HeosPlayer) -> Option<Self> {
        Some(Self::of(&player.client()?, player.pid()))
    }

    fn of(heos: &Arc<Heos>, pid: PlayerId) -> Self {
        Self {
            heos: Arc::downgrade(heos),
            pid,
        }
    }

    /// Run `read` with the state of the player, without a copy. `None` while the player is not
    /// known or the connection is closed. The state is locked while `read` runs: keep it short.
    pub fn with_player<T>(&self, read: impl FnOnce(&PlayerState) -> T) -> Option<T> {
        self.heos.upgrade()?.with_player(self.pid, read)
    }

    pub const fn pid(&self) -> PlayerId {
        self.pid
    }

    fn heos(&self) -> Result<Arc<Heos>, ServiceError> {
        self.heos
            .upgrade()
            .ok_or_else(|| denon_marantz::heos::Error::NotConnected.into())
    }

    /// Play, or pause what plays. An item that cannot pause is stopped.
    pub async fn play_pause(&self) -> Result<(), ServiceError> {
        let next = self
            .with_player(|p| match p.play_state {
                Some(PlayState::Play) if p.controls().pause => PlayState::Pause,
                Some(PlayState::Play) => PlayState::Stop,
                _ => PlayState::Play,
            })
            .unwrap_or(PlayState::Play);
        self.set_play_state(next).await
    }

    pub async fn set_play_state(&self, state: PlayState) -> Result<(), ServiceError> {
        Ok(self.heos()?.set_play_state(self.pid, state).await?)
    }

    /// Set the repeat mode, the shuffle mode, or both. The part that is `None` stays as it is.
    pub async fn set_play_mode(
        &self,
        repeat: Option<RepeatMode>,
        shuffle: Option<bool>,
    ) -> Result<(), ServiceError> {
        let now = self
            .with_player(PlayerState::play_mode)
            .flatten()
            .unwrap_or(PlayMode {
                repeat: RepeatMode::Off,
                shuffle: false,
            });
        let mode = PlayMode {
            repeat: repeat.unwrap_or(now.repeat),
            shuffle: shuffle.unwrap_or(now.shuffle),
        };
        Ok(self.heos()?.set_play_mode(self.pid, mode).await?)
    }

    pub async fn next(&self) -> Result<(), ServiceError> {
        if !self.with_player(|p| p.controls().next).unwrap_or(true) {
            return Err(ServiceError::Blocked(
                "next is not possible for this item".into(),
            ));
        }
        Ok(self.heos()?.play_next(self.pid).await?)
    }

    pub async fn previous(&self) -> Result<(), ServiceError> {
        if !self.with_player(|p| p.controls().previous).unwrap_or(true) {
            return Err(ServiceError::Blocked(
                "previous is not possible for this item".into(),
            ));
        }
        Ok(self.heos()?.play_previous(self.pid).await?)
    }
}

/// Whether a change can change what the media player of `pid` shows.
pub fn concerns(pid: PlayerId, change: &Change) -> bool {
    match change {
        Change::Players | Change::PlayersNotRead => true,
        Change::PlayState(p)
        | Change::NowPlaying(p)
        | Change::Progress(p)
        | Change::PlayMode(p) => *p == pid,
        _ => false,
    }
}

/// The wait before the next connect. Right after wake-up the network can be not ready: until
/// `fast_until`, an unreachable receiver is asked again every [`NETWORK_RETRY_INTERVAL`]. Else
/// `attempt` selects the wait in [`HEOS_WAITS`] and counts up.
fn retry_wait(failure: HeosFailure, fast_until: Instant, attempt: &mut usize) -> Duration {
    if failure == HeosFailure::Unreachable && Instant::now() + NETWORK_RETRY_INTERVAL < fast_until {
        return NETWORK_RETRY_INTERVAL;
    }
    let wait = HEOS_WAITS[(*attempt).min(HEOS_WAITS.len() - 1)];
    *attempt = attempt.saturating_add(1);
    wait
}

/// Open HEOS again each time its first connect failed. The library does not try again then.
/// Ends when the `Avr` is dropped. The first failure after the fast tries is a warning, as cover
/// art and the play buttons then do not use HEOS. After wake-up, the network is often not ready
/// for a short time: those failures are not warned about.
pub async fn keep_heos(avr: Arc<Avr>, name: String) {
    let fast_until = Instant::now() + NETWORK_RETRY_TIMEOUT;
    let mut status = avr.watch_heos_status();
    let mut attempt = 0;
    let mut warned = false;
    let mut last_error = None;
    loop {
        let failure = loop {
            let now = *status.borrow_and_update();
            if let HeosPlayerStatus::Failed(failure) = now {
                break failure;
            }
            if status.changed().await.is_err() {
                return;
            }
        };
        let wait = retry_wait(failure, fast_until, &mut attempt);
        // `attempt` counts the long waits only.
        if !warned && attempt > 0 {
            warned = true;
            let error = last_error
                .take()
                .map(|e: denon_marantz::Error| e.to_string())
                .or_else(|| avr.heos_start_error().map(|e| e.to_string()))
                .unwrap_or_else(|| format!("{failure:?}"));
            warn!("[{name}] HEOS not reachable, cover art and HEOS controls may not work: {error}");
        }
        sleep(wait).await;
        if let Err(e) = avr.heos_connect().await {
            debug!("[{name}] HEOS connect failed: {e}");
            if !warned {
                last_error = Some(e);
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The HEOS settings of the tests on this thread: the port of the fake. Without them, the
    /// start does not open HEOS.
    pub static TEST_HEOS: std::cell::Cell<Option<denon_marantz::heos::HeosConfig>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeHeos, HeosItem};

    async fn link(fake: &FakeHeos) -> (Arc<Heos>, Link) {
        let heos = Arc::new(
            Heos::connect_with("127.0.0.1", fake.config())
                .await
                .expect("connect"),
        );
        let pid = heos.players()[0].pid();
        let link = Link::of(&heos, pid);
        (heos, link)
    }

    #[test]
    fn only_an_unreachable_receiver_is_asked_again_soon() {
        let soon = Instant::now() + Duration::from_secs(10);
        let mut attempt = 0;
        assert_eq!(
            NETWORK_RETRY_INTERVAL,
            retry_wait(HeosFailure::Unreachable, soon, &mut attempt)
        );
        assert_eq!(0, attempt);
        assert_eq!(
            HEOS_WAITS[0],
            retry_wait(HeosFailure::Refused, soon, &mut attempt)
        );
        assert_eq!(1, attempt);
        // After the fast tries, an unreachable receiver waits too.
        let past = Instant::now();
        assert_eq!(
            HEOS_WAITS[1],
            retry_wait(HeosFailure::Unreachable, past, &mut attempt)
        );
        for _ in 0..10 {
            retry_wait(HeosFailure::Timeout, past, &mut attempt);
        }
        assert_eq!(
            HEOS_WAITS[HEOS_WAITS.len() - 1],
            retry_wait(HeosFailure::Timeout, past, &mut attempt)
        );
    }

    #[tokio::test]
    async fn a_station_that_cannot_pause_or_skip_is_stopped_and_not_skipped() {
        let fake = FakeHeos::start("127.0.0.1", HeosItem::station()).await;
        let (_heos, link) = link(&fake).await;
        assert!(matches!(link.next().await, Err(ServiceError::Blocked(_))));
        assert!(matches!(
            link.previous().await,
            Err(ServiceError::Blocked(_))
        ));
        link.play_pause().await.expect("stop");
        let received = fake.received();
        assert!(
            received
                .iter()
                .any(|l| l == "player/set_play_state?pid=7&state=stop"),
            "{received:?}"
        );
        assert!(
            !received.iter().any(|l| l.starts_with("player/play_next")
                || l.starts_with("player/play_previous")),
            "{received:?}"
        );
    }

    #[tokio::test]
    async fn play_pause_pauses_a_song_that_plays() {
        let fake = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
        let (_heos, link) = link(&fake).await;
        link.play_pause().await.expect("pause");
        assert!(
            fake.received()
                .iter()
                .any(|l| l == "player/set_play_state?pid=7&state=pause"),
            "{:?}",
            fake.received()
        );
    }

    #[tokio::test]
    async fn a_closed_connection_gives_no_state_and_an_error() {
        let fake = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
        let (heos, link) = link(&fake).await;
        assert!(link.with_player(|_| ()).is_some());
        drop(heos);
        assert!(link.with_player(|_| ()).is_none());
        assert!(link.play_pause().await.is_err());
    }
}
