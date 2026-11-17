// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! HEOS player of a receiver: what plays on network audio, and play, pause, stop, next and
//! previous for it. Volume and mute are not in HEOS: the zones share its volume.

use crate::config_store::ReceiverGeneration;
use crate::errors::ServiceError;
use denon_marantz_heos::{Error, Heos, HeosConfig, PlayState, PlayerId, PlayerState};
use log::{debug, info};
use std::io::ErrorKind;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::sync::watch;

/// The HEOS player of a receiver.
#[derive(Debug, Clone)]
pub struct Link {
    heos: Arc<Heos>,
    pid: PlayerId,
    players: watch::Receiver<Vec<PlayerState>>,
}

impl Link {
    pub fn heos(&self) -> &Heos {
        &self.heos
    }

    /// Run `read` with the state of the player, without a copy. `None` while the HEOS connection
    /// is down: the kept state can be old. Do not hold it across `.await`.
    pub fn with_player<T>(&self, read: impl FnOnce(Option<&PlayerState>) -> T) -> T {
        if self.heos.status() != denon_marantz_heos::HeosStatus::Connected {
            return read(None);
        }
        let players = self.players.borrow();
        read(players.iter().find(|p| p.pid() == self.pid))
    }

    pub const fn pid(&self) -> PlayerId {
        self.pid
    }

    /// Play, or pause what plays. An item that cannot pause is stopped.
    pub async fn play_pause(&self) -> Result<(), ServiceError> {
        let (state, controls) = self.with_player(|p| {
            p.map_or((None, denon_marantz_heos::Controls::ALL), |p| {
                (p.play_state, p.controls())
            })
        });
        let next = match state {
            Some(PlayState::Play) if controls.pause => PlayState::Pause,
            Some(PlayState::Play) => PlayState::Stop,
            _ => PlayState::Play,
        };
        self.set_play_state(next).await
    }

    pub async fn set_play_state(&self, state: PlayState) -> Result<(), ServiceError> {
        Ok(self.heos.set_play_state(self.pid, state).await?)
    }

    pub async fn next(&self) -> Result<(), ServiceError> {
        if !self.with_player(|p| p.is_none_or(|p| p.controls().next)) {
            return Err(ServiceError::Blocked(
                "next is not possible for this item".into(),
            ));
        }
        Ok(self.heos.play_next(self.pid).await?)
    }

    pub async fn previous(&self) -> Result<(), ServiceError> {
        if !self.with_player(|p| p.is_none_or(|p| p.controls().previous)) {
            return Err(ServiceError::Blocked(
                "previous is not possible for this item".into(),
            ));
        }
        Ok(self.heos.play_previous(self.pid).await?)
    }
}

/// Whether a change can change what the media player of `pid` shows.
pub fn concerns(pid: PlayerId, change: &denon_marantz_heos::Change) -> bool {
    use denon_marantz_heos::Change;
    match change {
        Change::Players | Change::PlayersNotRead => true,
        Change::PlayState(p) | Change::NowPlaying(p) | Change::Progress(p) => *p == pid,
        _ => false,
    }
}

/// A HEOS connection to a receiver.
#[derive(Debug)]
pub enum Connected {
    /// The player of the receiver is in the HEOS list.
    Player(Link),
    /// HEOS answers, but its list has no player with the address of the receiver. HEOS can need
    /// time to find its players: ask again later with [`find`].
    NoPlayer(Arc<Heos>),
}

/// Connect to HEOS on `host`, and find the player with the address of `host`.
pub async fn connect(host: &str) -> Result<Connected, Error> {
    let heos = Arc::new(Heos::connect_with(host, config()?).await?);
    Ok(match find(heos).await {
        Ok(link) => Connected::Player(link),
        Err(heos) => {
            info!("[{host}] HEOS has no player with this address yet");
            Connected::NoPlayer(heos)
        }
    })
}

/// The player with the address of the receiver in the HEOS list. Gives `heos` back if it is not
/// in the list.
pub async fn find(heos: Arc<Heos>) -> Result<Link, Arc<Heos>> {
    let ips = addresses(heos.host()).await;
    let Some(pid) = ips.into_iter().find_map(|ip| heos.player_by_ip(ip)) else {
        return Err(heos);
    };
    let players = heos.watch_players();
    Ok(Link { heos, pid, players })
}

/// Whether the receiver has HEOS. A receiver that answers on the HEOS port has it, also if its
/// player is not in the HEOS list yet. `None` if it is not known: HEOS did not answer, but did not
/// refuse either. The oldest receivers are not asked: they came before HEOS.
pub async fn probe(host: &str, receiver_type: Option<ReceiverGeneration>) -> Option<bool> {
    if receiver_type == Some(ReceiverGeneration::Avr) {
        return Some(false);
    }
    match connect(host).await {
        Ok(_) => Some(true),
        Err(e) if is_absent(&e) => Some(false),
        Err(e) => {
            debug!("[{host}] HEOS not checked: {e}");
            None
        }
    }
}

/// The receiver refuses the HEOS port: it has no HEOS.
pub fn is_absent(error: &Error) -> bool {
    matches!(error, Error::Network { source, .. } if source.kind() == ErrorKind::ConnectionRefused)
}

/// The IP addresses of a host name or an IP address.
async fn addresses(host: &str) -> Vec<IpAddr> {
    if let Ok(ip) = host.parse() {
        return vec![ip];
    }
    match tokio::net::lookup_host((host, 0)).await {
        Ok(found) => found.map(|a| a.ip()).collect(),
        Err(e) => {
            debug!("[{host}] address not resolved: {e}");
            Vec::new()
        }
    }
}

#[cfg(not(test))]
#[allow(clippy::unnecessary_wraps)] // The test version can fail.
fn config() -> Result<HeosConfig, Error> {
    Ok(HeosConfig::default())
}

/// The settings of the tests on this thread. Without them, HEOS is refused.
#[cfg(test)]
fn config() -> Result<HeosConfig, Error> {
    TEST_HEOS
        .with(std::cell::Cell::get)
        .ok_or_else(|| Error::Network {
            request: "HEOS connect".into(),
            source: ErrorKind::ConnectionRefused.into(),
        })
}

#[cfg(test)]
thread_local! {
    /// The HEOS settings of the tests on this thread: the port of the fake.
    pub static TEST_HEOS: std::cell::Cell<Option<HeosConfig>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeHeos, HeosItem};

    fn use_fake(fake: &FakeHeos) {
        TEST_HEOS.with(|slot| slot.set(Some(fake.config())));
    }

    #[tokio::test]
    async fn the_oldest_receivers_are_not_asked() {
        let fake = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
        use_fake(&fake);
        assert_eq!(
            Some(false),
            probe("127.0.0.1", Some(ReceiverGeneration::Avr)).await
        );
        assert_eq!(Vec::<String>::new(), fake.received());
    }

    #[tokio::test]
    async fn the_probe_finds_the_player_with_the_address_of_the_receiver() {
        let fake = FakeHeos::start("127.0.0.1", HeosItem::song()).await;
        use_fake(&fake);
        assert_eq!(
            Some(true),
            probe("127.0.0.1", Some(ReceiverGeneration::AvrX2016)).await
        );
    }

    #[tokio::test]
    async fn heos_without_the_player_of_the_receiver_is_still_heos() {
        let fake = FakeHeos::start("192.0.2.1", HeosItem::song()).await;
        use_fake(&fake);
        assert_eq!(Some(true), probe("127.0.0.1", None).await);
    }

    #[tokio::test]
    async fn a_refused_port_is_no_heos() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("address").port();
        drop(listener);
        TEST_HEOS.with(|slot| {
            slot.set(Some(HeosConfig {
                port,
                ..HeosConfig::default()
            }));
        });
        assert_eq!(Some(false), probe("127.0.0.1", None).await);
    }

    #[tokio::test]
    async fn a_station_that_cannot_pause_or_skip_is_stopped_and_not_skipped() {
        let fake = FakeHeos::start("127.0.0.1", HeosItem::station()).await;
        use_fake(&fake);
        let Connected::Player(link) = connect("127.0.0.1").await.expect("connect") else {
            panic!("no player");
        };
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
        use_fake(&fake);
        let Connected::Player(link) = connect("127.0.0.1").await.expect("connect") else {
            panic!("no player");
        };
        link.play_pause().await.expect("pause");
        assert!(
            fake.received()
                .iter()
                .any(|l| l == "player/set_play_state?pid=7&state=pause"),
            "{:?}",
            fake.received()
        );
    }
}
