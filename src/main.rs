use futures_util::{
    future::{FutureExt, TryFutureExt, BoxFuture},
    stream::{StreamExt, TryStreamExt},
};
use irc::client::prelude::{Client as IrcClient, Command, Config as IrcLibConfig};
use pin_utils::pin_mut;
use ruma_client::{
    api::r0::{
        filter::{FilterDefinition, RoomEventFilter, RoomFilter},
        sync::sync_events::{Filter, SetPresence},
    },
    identifiers::RoomId,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    convert::TryInto,
    error::Error,
    future::Future,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{oneshot, Mutex};

#[derive(Deserialize, Debug)]
struct Config {
    homeserver: String,
    // userid: String,
    token: String,
    room: String,
    #[serde(flatten)]
    irc: IrcConfig,
}

#[derive(Deserialize, Debug)]
struct IrcConfig {
    irc_server: String,
    irc_port: String,
    irc_tls: String,
    irc_nick: String,
    irc_channel: String,
}

async fn irc_stream<'a>(
    config: &'a IrcConfig,
) -> Result<
    (
        impl Fn(&str) -> Result<(), irc::error::Error> + 'a,
        impl (Fn(String, Duration) -> BoxFuture<'a, bool>) + 'a,
    ),
    Box<dyn Error>,
> {
    let mut client = IrcClient::from_config(IrcLibConfig {
        nickname: Some(config.irc_nick.clone()),
        server: Some(config.irc_server.clone()),
        port: Some(config.irc_port.parse()?),
        use_tls: Some(config.irc_tls.parse()?),
        channels: vec![config.irc_channel.clone()],
        realname: Some("IRC Bridge Monitor".to_owned()),
        ping_timeout: Some(30),
        ..Default::default()
    })
    .await?;
    client.identify()?;

    let mut stream = client.stream()?;

    let watches: Arc<Mutex<HashMap<String, oneshot::Sender<()>>>> = Default::default();

    let watches_task = watches.clone();
    tokio::spawn(async move {
        while let Some(message) = stream.next().await.transpose().expect("irc stream err") {
            if let Command::PRIVMSG(_, msg) = message.command {
                let mut ws = watches_task.lock().await;

                if let Some(tx) = ws.remove(&msg) {
                    tx.send(()).unwrap();
                }
            }
        }
    });

    let channel: &str = &config.irc_channel;

    let watches_task = watches.clone();
    Ok((
        move |text: &'_ str| client.send_privmsg(channel, text),
        move |text: String, timeout: Duration| {
            let watches_task = watches_task.clone();
            let (tx, rx) = oneshot::channel();
            async move {
                let mut ws = watches_task.lock().await;
                ws.insert(text.clone(), tx);
                drop(ws);

                if let Ok(result) = tokio::time::timeout(timeout, rx).await {
                    result.unwrap();
                    true
                } else {
                    let mut ws = watches_task.lock().await;
                    ws.remove(&text);
                    false
                }
            }
            .boxed()
        },
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = envy::prefixed("MATRIX_IRC_MONITOR_").from_env::<Config>()?;

    let f = irc_stream(&config.irc).await?;

    let homeserver_url = config.homeserver.parse()?;
    // let uid: UserId = config.userid.try_into()?;
    let room: RoomId = config.room.try_into()?;

    let session = ruma_client::Session {
        access_token: config.token,
        identification: None,
    };
    let client = ruma_client::Client::https(homeserver_url, Some(session));

    let mut stream = client.sync(
        Some(Filter::FilterDefinition(FilterDefinition {
            room: Some(RoomFilter {
                rooms: Some(vec![room]),
                timeline: Some(RoomEventFilter {
                    types: Some(vec!["m.room.message".to_owned()]),
                    ..Default::default()
                }),

                state: Some(RoomEventFilter::ignore_all()),
                ephemeral: Some(RoomEventFilter::ignore_all()),
                account_data: Some(RoomEventFilter::ignore_all()),
                ..Default::default()
            }),
            ..FilterDefinition::ignore_all()
        })),
        None,
        SetPresence::Offline,
        Some(Duration::from_secs(5)),
    );
    pin_mut!(stream);

    while let Some(response) = stream.try_next().await? {
        // Do something with the data in the response...
        if let Some((id, room)) = response.rooms.join.iter().next() {
            dbg!(&room.timeline.events);
        }
    }

    Ok(())
}
