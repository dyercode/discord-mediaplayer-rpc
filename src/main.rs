extern crate futures;
use anyhow::anyhow;
use dbus::arg;
use dbus::arg::PropMap;
use dbus::message::MatchRule;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;
use dbus::nonblock::{Proxy, SyncConnection};
use dbus_tokio::connection::{self, IOResource};
use discord_presence::Client;
use futures::{prelude::*, TryFutureExt};
use log::{debug, info};
use std::env;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use stream_cancel::{StreamExt, Tripwire};
use tokio::sync::mpsc::{Receiver, Sender};

const SERVICE: &str = "org.mpris.MediaPlayer2.audacious";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const _PROPERTY_INTERFACE_NAME: &str = "org.freedesktop.DBus.Properties";

const CLIENT_ID: u64 = 1048886631823843368; // should be safe to leave public.

mod keys {
    pub const TITLE: &str = "xesam:title";
    pub const ALBUM: &str = "xesam:album";
    pub const ARTIST: &str = "xesam:artist";
}

#[derive(Default, Debug)]
struct MediaInfo {
    title: String,
    artist: String,
    album: String,
}

impl Display for MediaInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let on = if self.album.is_empty() { "" } else { " on " };
        write!(f, "{} - {}{}{}", self.artist, self.title, on, self.album)
    }
}

fn parse_metadata(metadata: &PropMap) -> anyhow::Result<MediaInfo> {
    match (
        arg::prop_cast(metadata, keys::TITLE).cloned(),
        arg::prop_cast(metadata, keys::ALBUM).cloned(),
        arg::prop_cast::<Vec<String>>(metadata, keys::ARTIST).cloned(),
    ) {
        (None, None, None) => Err(anyhow!("no track data returned")),
        (title, album, artist) => Ok(MediaInfo {
            title: title.unwrap_or_default(),
            album: album.unwrap_or_default(),
            artist: artist.unwrap_or_default().join(" & "),
        }),
    }
}

fn parse_playback(playback: Option<String>) -> PlaybackStatus {
    match playback {
        None => PlaybackStatus::Closed,
        Some(s) if s == "Paused" => PlaybackStatus::Paused,
        Some(s) if s == "Playing" => PlaybackStatus::Playing,
        Some(s) if s == "Stopped" => PlaybackStatus::Stopped,
        Some(s) => unreachable!("guess I missed a status: `{}`", s),
    }
}

async fn read_metadata(proxy: &Proxy<'_, Arc<SyncConnection>>) -> anyhow::Result<MediaInfo> {
    proxy
        .get(PLAYER_INTERFACE, "Metadata")
        .map_err(|_| anyhow!("dbus error"))
        .and_then(|md: PropMap| async move { parse_metadata(&md) })
        .await
}

#[derive(Debug, PartialEq)]
enum PlaybackStatus {
    Stopped,
    Playing,
    Paused,
    Closed,
}

async fn read_playback_status(proxy: &Proxy<'_, Arc<SyncConnection>>) -> PlaybackStatus {
    parse_playback(proxy.get(PLAYER_INTERFACE, "PlaybackStatus").await.ok())
}

type PlayingMessage = (Option<MediaInfo>, PlaybackStatus);

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    debug!("started");
    let (resource, conn): (IOResource<SyncConnection>, Arc<SyncConnection>) =
        connection::new_session_sync()?;

    debug!("connection created");
    // The resource is a task that should be spawned onto a tokio compatible
    // reactor ASAP. If the resource ever finishes, you lost connection to D-Bus.
    tokio::spawn(async {
        let err = resource.await;
        debug!("panicking cause debus connection {}", err);
        panic!("Lost connection to D-Bus: {}", err);
    });

    debug!("connection spawned");
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .with_path("/org/mpris/MediaPlayer2");

    // Make a "proxy object" that contains the destination and path of our method call.
    let proxy: Proxy<Arc<SyncConnection>> = Proxy::new(
        SERVICE,
        "/org/mpris/MediaPlayer2",
        Duration::from_secs(5),
        conn.clone(),
    );

    let (tx, mut rx): (Sender<PlayingMessage>, Receiver<PlayingMessage>) =
        tokio::sync::mpsc::channel(25);

    debug!("channel created");

    let _discord_client = tokio::spawn(async move {
        let mut client = Client::new(CLIENT_ID);
        client.start();
        debug!("discord client started");
        while let Some(mi_mb) = rx.recv().await {
            match mi_mb {
                (Some(mi), PlaybackStatus::Playing) => {
                    let activity: Activity = mi.into();
                    let _ = client.set_activity(|act| match activity.state {
                        Some(album) => act.state(album).details(activity.details),
                        None => act.details(activity.details),
                    });
                }
                (Some(_), _) => {
                    let _ = client.clear_activity();
                }
                (None, _) => {
                    let _ = client.clear_activity();
                }
            }
        }
    });

    debug!("discord client spawned");

    // todo - set state at this app's startup.
    let (trigger, tripwire) = Tripwire::new();
    let (signal, stream) = conn.add_match(rule).await?.stream();
    let stream_fut = stream
        .take_until_if(tripwire)
        .for_each(|(_, _): (_, (String,))| {
            async {
                // todo - find way to verify that this is from audacious
                debug!("about to read a playback status");
                let status: PlaybackStatus = read_playback_status(&proxy).await;
                debug!("read a playback status");
                if let PlaybackStatus::Paused | PlaybackStatus::Playing = status {
                    let _ = read_metadata(&proxy)
                        .and_then(|mi| {
                            info!("{}", mi);
                            tx.send((Some(mi), status))
                                .map_err(|_| anyhow!("error sending metadata and status"))
                        })
                        .await;
                } else {
                    info!("not playing");
                    let _ = tx.send((None, status)).await;
                }
                tokio::task::yield_now().await
            }
        });

    // tokio::time::sleep(Duration::new(60, 0)).await;
    match env::args().nth(1) {
        Some(arg) if arg == "-d" => debug!("running in daemon mode"),
        _ => {
            debug!("running in console mode ");
            tokio::spawn(async move {
                let mut buffer = String::new();
                debug!("pausing forever (until newln)");
                let _ = std::io::stdin().read_line(&mut buffer);
                debug!("done waiting forever `{}`", buffer);
                let _ = conn.remove_match(signal.token()).await;
                drop(trigger);
            });
        }
    }
    stream_fut.await;
    debug!("future ended");
    Ok(())
}

struct Activity {
    state: Option<String>,
    details: String,
}

impl From<MediaInfo> for Activity {
    fn from(mi: MediaInfo) -> Self {
        match mi.album {
            a if a.is_empty() => Activity {
                state: None,
                details: format!("Playing {} - {}", mi.artist, mi.title),
            },
            album => Activity {
                state: Some(format!("From {}", album)),
                details: format!("Playing {} - {}", mi.artist, mi.title),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_has_album_as_state_when_present() {
        let media_info = MediaInfo {
            album: "album".to_owned(),
            artist: "artist".to_owned(),
            title: "title".to_owned(),
        };

        let result: Activity = media_info.into();
        assert_eq!(result.state, Some("From album".to_owned()));
    }

    #[test]
    fn activity_has_no_state_when_album_empty() {
        let media_info = MediaInfo {
            album: "".to_owned(),
            artist: "artist".to_owned(),
            title: "title".to_owned(),
        };

        let result: Activity = media_info.into();
        assert!(result.state.is_none());
    }

    #[test]
    fn parsing_playback_status_closed_when_no_value_present() {
        parse_playback(None);
    }

    #[test]
    fn parsing_playback_paused() {
        assert_eq!(
            parse_playback(Some("Paused".to_string())),
            PlaybackStatus::Paused
        );
    }

    #[test]
    fn parsing_playback_playing() {
        assert_eq!(
            parse_playback(Some("Playing".to_string())),
            PlaybackStatus::Playing
        );
    }

    #[test]
    fn parsing_playback_stopped() {
        assert_eq!(
            parse_playback(Some("Stopped".to_string())),
            PlaybackStatus::Stopped
        );
    }

    #[test]
    #[should_panic]
    fn parsing_playback_status_panics_when_unknown_status() {
        parse_playback(Some("Fish".to_owned()));
    }
}
