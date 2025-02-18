use std::time::Duration;
use std::net::SocketAddr;


pub struct Config {
    pub listen_addr: SocketAddr,
    pub pings: Vec<PingConfig>,
}

impl Config {
    pub fn new() -> Self
    {
        Self {
            listen_addr: ([127, 0, 0, 1], 3000).into(),
            pings: Vec::new(),
        }
    }

    pub fn ping(&mut self, token: &str, name: &str, period: Duration, grace: Duration)
    {
        self.pings.push(PingConfig {
            token: token.to_string(),
            name: name.to_string(),
            period,
            grace,
        });
    }
}


pub struct PingConfig {
    /// Token used in ping URL, unique identifier.
    pub token: String,
    /// Name of the ping to display on dashboard and in reports.
    pub name: String,
    /// Expected amount of time between pings.
    pub period: Duration,
    /// Amount of time after a missed deadline before this item is considered to be in error state.
    pub grace: Duration,
}
