use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::rules::{Action, Direction, Filter, RuleEngine, RuleSpec, DEFAULT_LOSS_SEED};

pub async fn run_control_socket(
    path: &Path,
    engine: Arc<Mutex<RuleEngine>>,
) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let listener = UnixListener::bind(path)?;
    info!(path = %path.display(), "Control socket listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, engine).await {
                warn!("Control connection error: {e}");
            }
        });
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    engine: Arc<Mutex<RuleEngine>>,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        let response = process_command(&line, &engine).await;
        writer.write_all(response.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}

async fn process_command(line: &str, engine: &Arc<Mutex<RuleEngine>>) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return "ERR empty command".to_string();
    }

    match parts[0] {
        "stats" => {
            let eng = engine.lock().await;
            format!("OK {}", eng.stats_json())
        }
        "rule" if parts.len() >= 2 => match parts[1] {
            "add" if parts.len() >= 3 => parse_rule_add(&parts[2..], engine).await,
            "list" => {
                let eng = engine.lock().await;
                let rules = eng.list_rules();
                if rules.is_empty() {
                    "OK []".to_string()
                } else {
                    let items: Vec<String> = rules
                        .iter()
                        .map(|r| {
                            let remaining = match r.remaining {
                                Some(n) => format!("{n}"),
                                None => "null".to_string(),
                            };
                            format!(
                                r#"{{"id":{},"direction":"{}","action":"{}","filter":"{}","remaining":{}}}"#,
                                r.id, r.direction, r.action, r.filter, remaining
                            )
                        })
                        .collect();
                    format!("OK [{}]", items.join(","))
                }
            }
            "clear" if parts.len() >= 3 => {
                if parts[2] == "all" {
                    engine.lock().await.clear_all();
                    "OK".to_string()
                } else {
                    match parts[2].parse::<u32>() {
                        Ok(id) => {
                            if engine.lock().await.clear_rule(id) {
                                "OK".to_string()
                            } else {
                                format!("ERR rule {id} not found")
                            }
                        }
                        Err(_) => "ERR invalid rule id".to_string(),
                    }
                }
            }
            _ => "ERR unknown rule subcommand".to_string(),
        },
        _ => "ERR unknown command".to_string(),
    }
}

async fn parse_rule_add(args: &[&str], engine: &Arc<Mutex<RuleEngine>>) -> String {
    if args.is_empty() {
        return "ERR missing action".to_string();
    }

    let (action, rest) = match args[0] {
        "drop" => (Action::Drop, &args[1..]),
        "delay" => {
            if args.len() < 2 {
                return "ERR delay requires milliseconds".to_string();
            }
            match args[1].parse::<u64>() {
                Ok(ms) => (Action::Delay(ms), &args[2..]),
                Err(_) => return "ERR invalid delay value".to_string(),
            }
        }
        "corrupt" => (Action::Corrupt, &args[1..]),
        other => return format!("ERR unknown action: {other}"),
    };

    let mut direction = Direction::Both;
    let mut filter = Filter::All;
    let mut min_size: usize = 0;
    let mut max_size: usize = 0;
    let mut skip: u32 = 0;
    let mut count: Option<u32> = None;
    let mut rate: Option<f64> = None;
    let mut seed: u64 = DEFAULT_LOSS_SEED;

    for &arg in rest {
        if let Some(val) = arg.strip_prefix("direction=") {
            direction = match val {
                "a_to_b" => Direction::AToB,
                "b_to_a" => Direction::BToA,
                "both" => Direction::Both,
                _ => return format!("ERR invalid direction: {val}"),
            };
        } else if let Some(val) = arg.strip_prefix("filter=") {
            filter = match val {
                "all" => Filter::All,
                _ if val.starts_with("cmd:") => {
                    let hex = val.strip_prefix("cmd:").expect("already checked prefix");
                    let hex = hex.strip_prefix("0x").unwrap_or(hex);
                    match u8::from_str_radix(hex, 16) {
                        Ok(cmd) => Filter::Command(cmd),
                        Err(_) => return format!("ERR invalid command hex: {hex}"),
                    }
                }
                _ => return format!("ERR invalid filter: {val}"),
            };
        } else if let Some(val) = arg.strip_prefix("min_size=") {
            match val.parse::<usize>() {
                Ok(n) => min_size = n,
                Err(_) => return format!("ERR invalid min_size: {val}"),
            }
        } else if let Some(val) = arg.strip_prefix("max_size=") {
            match val.parse::<usize>() {
                Ok(n) => max_size = n,
                Err(_) => return format!("ERR invalid max_size: {val}"),
            }
        } else if let Some(val) = arg.strip_prefix("skip=") {
            match val.parse::<u32>() {
                Ok(n) => skip = n,
                Err(_) => return format!("ERR invalid skip: {val}"),
            }
        } else if let Some(val) = arg.strip_prefix("count=") {
            match val.parse::<u32>() {
                Ok(n) => count = Some(n),
                Err(_) => return format!("ERR invalid count: {val}"),
            }
        } else if let Some(val) = arg.strip_prefix("rate=") {
            match val.parse::<f64>() {
                Ok(r) if (0.0..=1.0).contains(&r) => rate = Some(r),
                Ok(r) => return format!("ERR rate out of range (0.0..=1.0): {r}"),
                Err(_) => return format!("ERR invalid rate: {val}"),
            }
        } else if let Some(val) = arg.strip_prefix("seed=") {
            match val.parse::<u64>() {
                Ok(n) => seed = n,
                Err(_) => return format!("ERR invalid seed: {val}"),
            }
        } else {
            return format!("ERR unknown option: {arg}");
        }
    }

    // Deterministic count mode and probabilistic rate mode are mutually
    // exclusive: a single rule cannot be both bounded-by-count and stochastic.
    if count.is_some() && rate.is_some() {
        return "ERR count and rate are mutually exclusive".to_string();
    }

    let id = engine.lock().await.add_rule_spec(RuleSpec {
        direction,
        action,
        filter,
        min_size,
        max_size,
        skip,
        count,
        rate,
        seed,
    });
    format!("OK {id}")
}
