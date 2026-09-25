//! In-sandbox TCP -> unix socket forwarder for the egress proxy.
//! See `agent_adapters::sandbox::egress::netbridge_main`.

#[cfg(unix)]
fn main() {
    std::process::exit(agent_adapters::sandbox::egress::netbridge_main(std::env::args().skip(1).collect()));
}

#[cfg(not(unix))]
fn main() {
    eprintln!("agent-netbridge is only supported on unix");
    std::process::exit(125);
}
