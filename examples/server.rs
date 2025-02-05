use chrono::Local;
use env_logger::Builder;
use log::{error, info, Level, LevelFilter};
use std::io::Write;
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        error!("Usage: client <port>");
        return;
    }

    init_logger();
    let port = &args[1];
    let addr_string = format!("127.0.0.1:{}", port);
    let socket_addr = SocketAddr::from_str(&addr_string).unwrap();

    let mut server = UtpStream::bind(Some(socket_addr)).await;
    let mut buf = [0u8; 25]; // Adjust buffer size as needed

    // Use a single read instead of an explicit loop
    while let Ok(len) = server.read(&mut buf).await {
        if len == 0 {
            break; // Exit if the connection closes
        }
        info!(
            "Received {} bytes: {:?}",
            len,
            String::from_utf8_lossy(&buf[..len])
        );
    }
}
