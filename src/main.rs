/*
 * Copyright 2024 Oxide Computer Company
 */

use anyhow::{anyhow, bail, Result};
use dropshot::{
    endpoint, ApiDescription, ConfigDropshot, ConfigLogging,
    ConfigLoggingLevel, HttpError, HttpServerStarter, RequestContext,
};
use getopts::{Matches, Options};
use hyper::{Body, Response};
use slog::{crit, info, o, warn, Logger};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::result::Result as StdResult;
use std::sync::Arc;

mod sdr;

trait AnyhowHttpError<T> {
    fn or_500(self) -> StdResult<T, HttpError>;
    fn or_400(self) -> StdResult<T, HttpError>;
}

impl<T> AnyhowHttpError<T> for Result<T> {
    fn or_500(self) -> StdResult<T, HttpError> {
        self.map_err(|e| {
            let msg = format!("internal error: {}", e);
            HttpError::for_internal_error(msg)
        })
    }

    fn or_400(self) -> StdResult<T, HttpError> {
        self.map_err(|e| {
            HttpError::for_client_error(
                None,
                hyper::StatusCode::BAD_REQUEST,
                format!("request error: {}", e),
            )
        })
    }
}

struct Main {
    sdr: sdr::SdrTail,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut opts = Options::new();

    opts.optopt("b", "", "bind address:port", "ADDRESS:PORT");

    let p = match opts.parse(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ERROR: usage: {}", e);
            eprintln!("       {}", opts.usage("usage"));
            std::process::exit(1);
        }
    };

    if p.free.len() != 1 {
        bail!("specify data file name");
    }
    let file = PathBuf::from(&p.free[0]);

    let cfglog =
        ConfigLogging::StderrTerminal { level: ConfigLoggingLevel::Info };
    let log = cfglog.to_logger("temperature-exporter")?;

    if let Err(e) = run(log.clone(), p, file).await {
        crit!(log, "critical failure: {:?}", e);
        std::process::exit(1);
    }

    Ok(())
}

struct EmitterStat {
    name: String,
    typ: String,
    desc: String,
    label_name: String,
}

struct Emitter {
    typedefs: HashMap<String, EmitterStat>,
    printed: HashSet<String>,
    out: String,
}

impl Emitter {
    fn new() -> Emitter {
        Emitter {
            typedefs: HashMap::new(),
            printed: HashSet::new(),
            out: String::new(),
        }
    }

    fn define(
        &mut self,
        stat_name: &str,
        stat_type: &str,
        stat_desc: &str,
        label_name: &str,
    ) {
        self.typedefs.insert(
            stat_name.to_string(),
            EmitterStat {
                name: stat_name.to_string(),
                typ: stat_type.to_string(),
                desc: stat_desc.to_string(),
                label_name: label_name.to_string(),
            },
        );
    }

    fn emit_header(&mut self, stat_name: &str) {
        if self.printed.contains(stat_name) {
            return;
        }

        let es = self.typedefs.get(stat_name).unwrap();

        self.out += &format!("# TYPE {} {}\n", es.name, es.typ);
        self.out += &format!("# HELP {} {}\n", es.name, es.desc);

        self.printed.insert(stat_name.to_string());
    }

    fn emit_i64(&mut self, stat_name: &str, label_value: &str, val: i64) {
        self.emit_header(stat_name);

        let es = self.typedefs.get(stat_name).unwrap();
        self.out += &format!(
            "{}{{{}=\"{}\"}}\t{}\n",
            es.name, es.label_name, label_value, val
        );
    }

    fn emit_f32(&mut self, stat_name: &str, label_value: &str, val: f32) {
        self.emit_header(stat_name);

        let es = self.typedefs.get(stat_name).unwrap();
        self.out += &format!(
            "{}{{{}=\"{}\"}}\t{}\n",
            es.name, es.label_name, label_value, val
        );
    }

    fn out(&self) -> &str {
        self.out.as_str()
    }
}

#[endpoint {
    method = GET,
    path = "/metrics",
}]
async fn metrics(
    rc: RequestContext<Arc<Main>>,
) -> StdResult<Response<Body>, HttpError> {
    let log = &rc.log;
    let m = rc.context();

    // let mut k = m.kstat.lock().unwrap();

    let mut e = Emitter::new();

    e.define(
        "temperature_degrees_celsius",
        "gauge",
        "temperature in degrees celsius",
        "location",
    );

    e.define(
        "temperature_humidity_percent",
        "gauge",
        "relative humidity",
        "location",
    );

    e.define(
        "temperature_battery_ok",
        "gauge",
        "sensor battery health",
        "location",
    );

    {
        for (id, r) in m.sdr.values() {
            let location = match id.as_str() {
                "acurite-tower-00005019-c" => "garage-door",
                "acurite-tower-00007276-b" => "interior-door",
                "acurite-tower-00011771-a" => "machine-room",
                _ => {
                    warn!(log, "new temperature sensor? {id:?} -> {r:?}");
                    continue;
                }
            };

            e.emit_f32(
                "temperature_degrees_celsius",
                location,
                r.temperature_C,
            );
            e.emit_f32("temperature_humidity_percent", location, r.humidity);
            e.emit_i64("temperature_battery_ok", location, r.battery_ok);
        }
    }

    Ok(Response::builder()
        .status(200)
        .header("content-type", "text/plain")
        .body(Body::from(e.out().to_string()))?)
}

async fn run(log: Logger, p: Matches, file: PathBuf) -> Result<()> {
    let bind = p.opt_str("b").unwrap_or(String::from("0.0.0.0:4547"));

    let mut api = ApiDescription::new();
    api.register(metrics).unwrap();

    let cfg =
        ConfigDropshot { bind_address: bind.parse()?, ..Default::default() };

    let m = Arc::new(Main {
        sdr: sdr::SdrTail::new(log.new(o!("component" => "sdrtail")), file)?,
    });

    let server = HttpServerStarter::new(&cfg, api, m, &log)
        .map_err(|e| anyhow!("server startup failure: {e:?}"))?;

    info!(log, "listening on {:?}", cfg.bind_address);
    let server_task = server.start();

    server_task.await.map_err(|e| anyhow!("failure to wait: {:?}", e))
}
