use dbus::arg;
use dbus::arg::PropMap;
use dbus::message::MatchRule;
use dbus::nonblock::stdintf::org_freedesktop_dbus::Properties;
use dbus::nonblock::{Proxy, SyncConnection};
use dbus_tokio::connection::{self, IOResource};
use discord_presence::Client;
use futures::prelude::*;
use std::fmt::Display;
use std::sync::Arc;
use std::time::Duration;
use stream_cancel::{StreamExt, Tripwire};
use tokio::sync::mpsc::{Receiver, Sender};
use log::info;

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
        write!(f, "{} - {} on {}", self.artist, self.title, self.album)
    }
}

fn parse_metadata(metadata: &PropMap) -> Option<MediaInfo> {
    match (
        arg::prop_cast(metadata, keys::TITLE).cloned(),
        arg::prop_cast(metadata, keys::ALBUM).cloned(),
        arg::prop_cast::<Vec<String>>(metadata, keys::ARTIST).cloned(),
    ) {
        (None, None, None) => None,
        (title, album, artist) => Some(MediaInfo {
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

async fn read_metadata(proxy: &Proxy<'_, Arc<SyncConnection>>) -> Option<MediaInfo> {
    proxy
        .get(PLAYER_INTERFACE, "Metadata")
        .await
        .map(|md| parse_metadata(&md))
        .ok()
        .flatten()
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
    let (resource, conn): (IOResource<SyncConnection>, Arc<SyncConnection>) =
        connection::new_session_sync()?;

    // The resource is a task that should be spawned onto a tokio compatible
    // reactor ASAP. If the resource ever finishes, you lost connection to D-Bus.
    tokio::spawn(async {
        let err = resource.await;
        panic!("Lost connection to D-Bus: {}", err);
    });

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

    let _discord_client = tokio::spawn(async move {
        let mut client = Client::new(CLIENT_ID);
        let _ = client.start();
        while let Some(mi_mb) = rx.recv().await {
            // todo - refactor out all the formatting.
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

    // todo - set state at this app's startup.
    let (trigger, tripwire) = Tripwire::new();
    let (signal, stream) = conn.add_match(rule).await?.stream();
    let stream_fut = stream
        .take_until_if(tripwire)
        .for_each(|(_, _): (_, (String,))| {
            async {
                // todo - find way to verify that this is from audacious
                let status: PlaybackStatus = read_playback_status(&proxy).await;
                if let PlaybackStatus::Paused | PlaybackStatus::Playing = status {
                    if let Some(mi) = read_metadata(&proxy).await {
                        info!("{}", mi);
                        let _ = tx.send((Some(mi), status)).await;
                    }
                } else {
                    info!("not playing");
                    let _ = tx.send((None, status)).await;
                }
                tokio::task::yield_now().await
            }
        });

    // tokio::time::sleep(Duration::new(60, 0)).await;
    tokio::spawn(async move {
        let mut buffer = String::new();
        let _ = std::io::stdin().read_line(&mut buffer);
        let _ = conn.remove_match(signal.token()).await;
        drop(trigger);
    });
    stream_fut.await;
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
