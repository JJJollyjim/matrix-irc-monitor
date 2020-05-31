use serde::Deserialize;
use std::error::Error;

#[derive(Deserialize, Debug)]
struct Config {
    homeserver: String,
    userid: String,
    password: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = envy::prefixed("MATRIX_IRC_MONITOR_").from_env::<Config>()?;

    let homeserver_url = config.homeserver.parse()?;
    let client = ruma_client::Client::https(homeserver_url, None);

    let session = client
        .log_in(config.userid, config.password, None, None)
        .await?;

    println!("{}", session.access_token);
    Ok(())
}
