use chrono::Local;
use env_logger::Builder;
use futures_util::io::AsyncWriteExt;
use log::{error, info, Level, LevelFilter};
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use utp::stream::UtpStream;

fn init_logger() {
    Builder::new()
        .format(|buf, record| {
            let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
            let level = colored_level(record.level());
            let file_path = record.file().unwrap_or("unknown");
            let line_number = record.line().unwrap_or(0);

            // Extract the relative path from "src/"
            let trimmed_path = file_path
                .rsplit_once("src/")
                .map(|(_, relative)| format!("src/{}", relative))
                .unwrap_or_else(|| file_path.to_string());

            writeln!(
                buf,
                "[{}] [{}] [{}:{}] - {}",
                timestamp,
                level,
                trimmed_path,
                line_number,
                record.args()
            )
        })
        .filter(None, LevelFilter::Info)
        .init();
}

fn colored_level(level: Level) -> String {
    match level {
        Level::Error => format!("\x1b[31m{}\x1b[0m", level), // Red
        Level::Warn => format!("\x1b[33m{}\x1b[0m", level),  // Yellow
        Level::Info => format!("\x1b[32m{}\x1b[0m", level),  // Green
        Level::Debug => format!("\x1b[34m{}\x1b[0m", level), // Blue
        Level::Trace => format!("\x1b[35m{}\x1b[0m", level), // Magenta
    }
}
#[tokio::main]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        error!("Usage: client <port>");
        std::process::exit(1);
    }
    init_logger();
    let port: &String = &args[1];
    let addr = format!("127.0.0.1:{}", &port);
    let client_addr = SocketAddr::from_str(&addr).unwrap();
    let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();

    let mut client_stream = UtpStream::bind(Some(client_addr)).await;
    client_stream.connect(server_addr).await?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    while let Ok(Some(message)) = reader.next_line().await {
        client_stream.write_all(message.as_bytes()).await?
    }
    Ok(())
}
