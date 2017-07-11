use super::Source;
use metric;
use protocols::graphite::parse_graphite;
use slog;
use source::internal::report_telemetry;
use std::io::BufReader;
use std::io::prelude::*;
use std::net::{TcpListener, TcpStream};
use std::net::ToSocketAddrs;
use std::str;
use std::sync;
use std::sync::Arc;
use std::thread;
use util;
use util::send;

pub struct Graphite {
    log: slog::Logger,
    chans: util::Channel,
    host: String,
    port: u16,
    tags: Arc<metric::TagMap>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GraphiteConfig {
    pub host: String,
    pub port: u16,
    pub tags: metric::TagMap,
    pub forwards: Vec<String>,
    pub config_path: Option<String>,
}

impl Default for GraphiteConfig {
    fn default() -> GraphiteConfig {
        GraphiteConfig {
            host: "localhost".to_string(),
            port: 2003,
            tags: metric::TagMap::default(),
            forwards: Vec::new(),
            config_path: Some("sources.graphite".to_string()),
        }
    }
}

impl Graphite {
    pub fn new(
        chans: util::Channel,
        config: GraphiteConfig,
        log: slog::Logger,
    ) -> Graphite {
        Graphite {
            log: log,
            chans: chans,
            host: config.host,
            port: config.port,
            tags: Arc::new(config.tags),
        }
    }
}

fn handle_tcp(
    chans: util::Channel,
    tags: Arc<metric::TagMap>,
    listner: TcpListener,
    log: slog::Logger,
) -> thread::JoinHandle<()> {
    let log_mtx = sync::Mutex::new(log);
    thread::spawn(move || for stream in listner.incoming() {
        if let Ok(stream) = stream {
            let _log = log_mtx.lock().unwrap().new(o!(
                              "peer_addr" => format!("{:?}", stream.peer_addr()),
                              "local_addr" => format!("{:?}", stream.local_addr()),
                          ));
            report_telemetry("cernan.graphite.new_peer", 1.0);
            let tags = tags.clone();
            let chans = chans.clone();
            thread::spawn(move || { handle_stream(chans, tags, stream, _log); });
        }
    })
}


fn handle_stream(
    mut chans: util::Channel,
    tags: Arc<metric::TagMap>,
    stream: TcpStream,
    log: slog::Logger,
) {
    debug!(
        log,
        "new peer at {:?} | local addr for peer {:?}",
        stream.peer_addr(),
        stream.local_addr()
    );
    let mut line = String::new();
    let mut res = Vec::new();
    let mut line_reader = BufReader::new(stream);
    let basic_metric = Arc::new(Some(
        metric::Telemetry::default().overlay_tags_from_map(&tags),
    ));
    while let Some(len) = line_reader.read_line(&mut line).ok() {
        if len > 0 {
            if parse_graphite(&line, &mut res, basic_metric.clone()) {
                report_telemetry("cernan.graphite.packet", 1.0);
                for m in res.drain(..) {
                    send(&mut chans, metric::Event::Telemetry(Arc::new(Some(m))));
                }
                line.clear();
            } else {
                report_telemetry("cernan.graphite.bad_packet", 1.0);
                error!(log, "bad packet: {}", line);
                line.clear();
            }
        } else {
            break;
        }
    }
}

impl Source for Graphite {
    fn run(&mut self) {
        let mut joins = Vec::new();

        let addrs = (self.host.as_str(), self.port).to_socket_addrs();
        match addrs {
            Ok(ips) => {
                let ips: Vec<_> = ips.collect();
                for addr in ips {
                    let listener =
                        TcpListener::bind(addr).expect("Unable to bind to TCP socket");
                    let chans = self.chans.clone();
                    let tags = self.tags.clone();
                    info!(self.log, "server started on {:?} {}", addr, self.port);
                    let _log = self.log.new(o!(
                        "server_addr" => format!("{}", addr),
                        "port" => format!("{}", self.port),
                    ));
                    joins.push(thread::spawn(
                        move || handle_tcp(chans, tags, listener, _log),
                    ));
                }
            }
            Err(e) => {
                info!(
                    self.log,
                    "Unable to perform DNS lookup on host {} with error {}",
                    self.host,
                    e
                );
            }
        }

        // TODO thread spawn trick, join on results
        for jh in joins {
            // TODO Having sub-threads panic will not cause a bubble-up if that
            // thread is not the currently examined one. We're going to have to have
            // some manner of sub-thread communication going on.
            jh.join().expect("Uh oh, child thread panicked!");
        }
    }
}
